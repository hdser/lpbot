//! `lpbot select`: choosing what trades, the same way for a rule's settings
//! and for an agent.
//!
//! Two steps. `walk-forward` is the practice step for rules: over the runs
//! of a results folder it picks, period by period, the setting that did
//! best over the periods before, and reports what the pick then made in
//! the period it had not seen, against cash, against the average setting
//! and against the best setting chosen with hindsight. `ladder` is the
//! step to real positions: it reads forward runs (windows open at the end,
//! rules and agents alike), holds each against the gates of a ladder file,
//! and shares a track's capital among those that pass. Nothing here sends
//! a transaction; the plan it writes is what a live run would take.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use lpbot_sim::tables::{read_table, table_path, Column, Table};
use serde::{Deserialize, Serialize};

const DAY: u64 = 86_400;

/// How far back a pick looks: a number of periods, or every period so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Folds(usize),
    All,
}

impl Fit {
    fn name(self) -> String {
        match self {
            Fit::Folds(n) => format!("best of the last {n}"),
            Fit::All => "best so far".to_string(),
        }
    }
}

/// `3,6,all` as fits.
pub fn parse_fits(text: &str) -> Result<Vec<Fit>> {
    let fits = text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| match part {
            "all" => Ok(Fit::All),
            number => number
                .parse::<usize>()
                .ok()
                .filter(|n| *n > 0)
                .map(Fit::Folds)
                .ok_or_else(|| {
                    anyhow!("`{part}` is not a number of periods above 0, or `all`; try 3,6,all")
                }),
        })
        .collect::<Result<Vec<_>>>()?;
    if fits.is_empty() {
        bail!("no fit given; try 3,6,all");
    }
    Ok(fits)
}

/// One run as the selection sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub run_id: String,
    pub experiment: String,
    pub pool: String,
    pub value_token: String,
    pub sweep: String,
    pub window_from: String,
    pub window_to: String,
    /// Its window is open at the end: a run at the head of the chain.
    pub forward: bool,
    /// The agent that decided it, with its model: `None` for a rule.
    pub agent: Option<(String, String)>,
    pub result_pct: f64,
    pub result_pct_usd: f64,
    pub hold_usd_pct: f64,
    pub campaigns: i64,
    pub max_drawdown_pct: f64,
    pub halted: bool,
    /// The run's equity in the value token, by time.
    pub curve: Vec<(u64, f64)>,
}

impl Candidate {
    /// What tells this candidate from the others of its group.
    pub fn label(&self) -> String {
        match &self.agent {
            Some((name, model)) => format!("agent {name} ({model})"),
            None => self.sweep.clone(),
        }
    }

    /// Days of the run its curve covers.
    pub fn days(&self) -> f64 {
        match (self.curve.first(), self.curve.last()) {
            (Some(first), Some(last)) => (last.0 - first.0) as f64 / DAY as f64,
            _ => 0.0,
        }
    }
}

fn cell_text(table: &Table, name: &str) -> Option<String> {
    match table.column(name)? {
        Column::Text(values) => values.first().cloned().flatten(),
        _ => None,
    }
}

fn cell_float(table: &Table, name: &str) -> Option<f64> {
    match table.column(name)? {
        Column::Float(values) => values.first().copied().flatten(),
        Column::Int(values) => values.first().copied().flatten().map(|v| v as f64),
        _ => None,
    }
}

fn floats(table: &Table, name: &str) -> Vec<Option<f64>> {
    match table.column(name) {
        Some(Column::Float(values)) => values.clone(),
        Some(Column::Int(values)) => values.iter().map(|v| v.map(|v| v as f64)).collect(),
        _ => Vec::new(),
    }
}

fn texts(table: &Table, name: &str) -> Vec<Option<String>> {
    match table.column(name) {
        Some(Column::Text(values)) => values.clone(),
        _ => Vec::new(),
    }
}

/// Every run written under `results`, with its equity curve, sorted by
/// run id. `experiment` keeps only the experiments whose name starts with
/// it.
pub fn read_candidates(results: &Path, experiment: Option<&str>) -> Result<Vec<Candidate>> {
    let folder = results.join("runs");
    let entries = std::fs::read_dir(&folder).with_context(|| {
        format!(
            "{} holds no runs; `lpbot run <experiment> --results-dir {}` writes them",
            folder.display(),
            results.display()
        )
    })?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "parquet"))
        .collect();
    paths.sort();
    let mut candidates = Vec::new();
    for path in paths {
        let runs = read_table(&path)?;
        let need_text = |name: &str| {
            cell_text(&runs, name).ok_or_else(|| anyhow!("{} has no `{name}`", path.display()))
        };
        let need_float = |name: &str| {
            cell_float(&runs, name).ok_or_else(|| anyhow!("{} has no `{name}`", path.display()))
        };
        let name = need_text("experiment")?;
        if experiment.is_some_and(|prefix| !name.starts_with(prefix)) {
            continue;
        }
        let run_id = need_text("run_id")?;
        let config: serde_yaml::Value = cell_text(&runs, "config_yaml")
            .and_then(|yaml| serde_yaml::from_str(&yaml).ok())
            .unwrap_or_default();
        let forward = config
            .get("window")
            .and_then(|w| w.get("to"))
            .and_then(serde_yaml::Value::as_str)
            == Some("open");
        let agent = config
            .get("strategies")
            .and_then(|s| s.get(0))
            .and_then(|s| s.get("agent"))
            .and_then(|agent| {
                let text = |key: &str| {
                    agent
                        .get(key)
                        .and_then(serde_yaml::Value::as_str)
                        .map(str::to_string)
                };
                // The mandate is part of what the agent is.
                let model = text("model").unwrap_or_else(|| "unknown".into());
                Some((
                    text("name")?,
                    match text("mandate") {
                        Some(mandate) => format!("{model}, {mandate}"),
                        None => model,
                    },
                ))
            });
        let series = read_table(&table_path(results, "series", &run_id))?;
        let curve: Vec<(u64, f64)> = floats(&series, "time")
            .into_iter()
            .zip(floats(&series, "equity"))
            .filter_map(|(time, equity)| Some((time? as u64, equity?)))
            .collect();
        candidates.push(Candidate {
            experiment: name,
            pool: need_text("pool")?,
            value_token: need_text("value_token")?,
            sweep: cell_text(&runs, "sweep").unwrap_or_default(),
            window_from: need_text("window_from")?,
            window_to: need_text("window_to")?,
            forward,
            agent,
            result_pct: need_float("result_pct")?,
            result_pct_usd: need_float("result_pct_usd")?,
            hold_usd_pct: need_float("hold_value_pct_usd")?,
            campaigns: need_float("campaigns")? as i64,
            max_drawdown_pct: need_float("max_drawdown_pct")?,
            halted: matches!(
                runs.column("halted_by_loss_limit"),
                Some(Column::Flag(values)) if values.first() == Some(&Some(true))
            ),
            curve,
            run_id,
        });
    }
    Ok(candidates)
}

/// The curve's value at `time`: its last point at or before it, or its
/// first point for a time before the curve begins.
fn value_at(curve: &[(u64, f64)], time: u64) -> Option<f64> {
    let after = curve.partition_point(|point| point.0 <= time);
    curve.get(after.saturating_sub(1)).map(|point| point.1)
}

/// The curve's change over each of `folds` periods of `fold` seconds from
/// `start`, as fractions.
pub fn fold_returns(curve: &[(u64, f64)], start: u64, fold: u64, folds: usize) -> Vec<f64> {
    (0..folds as u64)
        .map(|k| {
            let from = value_at(curve, start + k * fold);
            let to = value_at(curve, start + (k + 1) * fold);
            match from.zip(to) {
                Some((from, to)) if from > 0.0 => to / from - 1.0,
                _ => 0.0,
            }
        })
        .collect()
}

fn compound(returns: impl IntoIterator<Item = f64>) -> f64 {
    returns.into_iter().fold(1.0, |total, r| total * (1.0 + r)) - 1.0
}

/// The candidate a fit picks for period `fold` from the periods before
/// it: the one that made most over them, or `None`, cash, when even the
/// best of them lost or made nothing. Ties go to the first.
pub fn pick(returns: &[Vec<f64>], fit: Fit, fold: usize) -> Option<usize> {
    let from = match fit {
        Fit::Folds(n) => fold.saturating_sub(n),
        Fit::All => 0,
    };
    let mut best: Option<(usize, f64)> = None;
    for (index, candidate) in returns.iter().enumerate() {
        let made = compound(candidate[from..fold].iter().copied());
        if best.is_none_or(|(_, most)| made > most) {
            best = Some((index, made));
        }
    }
    best.filter(|(_, made)| *made > 0.0).map(|(index, _)| index)
}

/// What one way of picking made on the periods it had not seen.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleScore {
    pub fit: Fit,
    /// Compounded over the scored periods, in percent.
    pub result_pct: f64,
    /// Each scored period's result, in percent, and the candidate picked
    /// for it (`None`: cash).
    pub fold_pct: Vec<f64>,
    pub picks: Vec<Option<usize>>,
    /// The candidate it picks for the period after the last one.
    pub next: Option<usize>,
}

/// A group of like-for-like candidates scored walk-forward.
#[derive(Debug, Clone, PartialEq)]
pub struct WalkForward {
    pub folds: usize,
    /// The first period that is scored: the ones before it are only fitted on.
    pub first_scored: usize,
    pub rules: Vec<RuleScore>,
    /// The average candidate over the scored periods, in percent.
    pub average_pct: f64,
    /// The candidate that made most over the scored periods, and what it made.
    pub hindsight: usize,
    pub hindsight_pct: f64,
    /// Candidates that ended the scored periods above where they began.
    pub above_zero: usize,
}

impl WalkForward {
    /// Whether a way of picking held up: it made money on the periods it
    /// had not seen, and more than the average candidate did.
    pub fn holds_up(&self, rule: &RuleScore) -> bool {
        rule.result_pct > 0.0 && rule.result_pct > self.average_pct
    }
}

/// Scores each fit on `returns` (one list of period returns a candidate).
/// Every fit is scored on the same periods: those from the longest fit on,
/// and from `min_fit` on for `Fit::All`. `None` when no period is left to
/// score.
pub fn walk_forward(returns: &[Vec<f64>], fits: &[Fit], min_fit: usize) -> Option<WalkForward> {
    let folds = returns.first()?.len();
    if returns.iter().any(|r| r.len() != folds) {
        return None;
    }
    let first_scored = fits
        .iter()
        .map(|fit| match fit {
            Fit::Folds(n) => *n,
            Fit::All => min_fit.max(1),
        })
        .max()?;
    if first_scored >= folds {
        return None;
    }
    let scored = first_scored..folds;
    let rules = fits
        .iter()
        .map(|&fit| {
            let picks: Vec<Option<usize>> = scored
                .clone()
                .map(|fold| pick(returns, fit, fold))
                .collect();
            let fold_returns: Vec<f64> = scored
                .clone()
                .zip(&picks)
                .map(|(fold, pick)| pick.map_or(0.0, |index| returns[index][fold]))
                .collect();
            RuleScore {
                fit,
                result_pct: compound(fold_returns.iter().copied()) * 100.0,
                fold_pct: fold_returns.iter().map(|r| r * 100.0).collect(),
                picks,
                next: pick(returns, fit, folds),
            }
        })
        .collect();
    let count = returns.len() as f64;
    let average = compound(
        scored
            .clone()
            .map(|fold| returns.iter().map(|r| r[fold]).sum::<f64>() / count),
    );
    let totals: Vec<f64> = returns
        .iter()
        .map(|r| compound(r[scored.clone()].iter().copied()))
        .collect();
    let (hindsight, most) =
        totals
            .iter()
            .copied()
            .enumerate()
            .fold((0, f64::NEG_INFINITY), |best, (index, total)| {
                if total > best.1 {
                    (index, total)
                } else {
                    best
                }
            });
    Some(WalkForward {
        folds,
        first_scored,
        rules,
        average_pct: average * 100.0,
        hindsight,
        hindsight_pct: most * 100.0,
        above_zero: totals.iter().filter(|total| **total > 0.0).count(),
    })
}

/// What `lpbot select walk-forward` is asked.
pub struct WalkForwardArgs<'a> {
    pub results: &'a Path,
    pub experiment: Option<&'a str>,
    /// Length of a period, in seconds.
    pub fold: u64,
    pub fits: &'a [Fit],
    pub min_fit: usize,
    /// A swept setting, such as `regime`: the runs that share its value
    /// are averaged and the pick is among its values, not among single
    /// runs. `None` picks among the runs.
    pub by: Option<&'a str>,
    /// Compare over every pool at once instead of pool by pool: a value of
    /// `by` is then judged on the average of its runs on all of them.
    pub across_pools: bool,
}

/// A run's swept settings, as the `runs` table holds them: `a=1; b=x`.
fn sweep_parts(sweep: &str) -> Vec<(&str, &str)> {
    sweep
        .split("; ")
        .filter_map(|part| part.split_once('='))
        .collect()
}

/// What is chosen among: runs, or the values of one swept setting with
/// the runs that share each. Runs are named by the settings that differ
/// between them.
pub fn units<'a>(
    group: &[&'a Candidate],
    by: Option<&str>,
) -> Result<Vec<(String, Vec<&'a Candidate>)>> {
    if let Some(key) = by {
        let mut units: Vec<(String, Vec<&Candidate>)> = Vec::new();
        for &candidate in group {
            let value = sweep_parts(&candidate.sweep)
                .into_iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value)
                .ok_or_else(|| {
                    anyhow!(
                        "run {} does not sweep `{key}`; --by takes a setting every run of the \
                         experiment sweeps",
                        candidate.run_id
                    )
                })?;
            let label = format!("{key}={value}");
            match units.iter_mut().find(|(name, _)| *name == label) {
                Some((_, members)) => members.push(candidate),
                None => units.push((label, vec![candidate])),
            }
        }
        return Ok(units);
    }
    Ok(group
        .iter()
        .zip(differing(group))
        .map(|(&candidate, what)| (format!("{} ({what})", candidate.run_id), vec![candidate]))
        .collect())
}

/// What tells each run of a group from the others: the swept settings that
/// take more than one value in the group; for an agent, or when nothing
/// differs, its label.
pub fn differing(group: &[&Candidate]) -> Vec<String> {
    let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for candidate in group {
        for (name, value) in sweep_parts(&candidate.sweep) {
            let values = seen.entry(name).or_default();
            if !values.contains(&value) {
                values.push(value);
            }
        }
    }
    group
        .iter()
        .map(|candidate| {
            let parts: Vec<String> = sweep_parts(&candidate.sweep)
                .into_iter()
                .filter(|(name, _)| seen.get(name).is_some_and(|values| values.len() > 1))
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            if candidate.agent.is_some() || parts.is_empty() {
                candidate.label()
            } else {
                parts.join("; ")
            }
        })
        .collect()
}

/// Scores every like-for-like group of runs under a results folder
/// walk-forward and returns the report.
pub fn walk_forward_report(args: &WalkForwardArgs) -> Result<String> {
    let candidates = read_candidates(args.results, args.experiment)?;
    if candidates.is_empty() {
        bail!(
            "no runs under {}{}",
            args.results.display(),
            args.experiment
                .map(|e| format!(" of an experiment starting with `{e}`"))
                .unwrap_or_default()
        );
    }
    // Like for like: the same pool, value token and window; over every
    // pool at once, the same window.
    let mut groups: BTreeMap<(String, String, String, String), Vec<&Candidate>> = BTreeMap::new();
    for candidate in &candidates {
        let (pool, value) = if args.across_pools {
            (String::new(), String::new())
        } else {
            (candidate.pool.clone(), candidate.value_token.clone())
        };
        groups
            .entry((
                pool,
                value,
                candidate.window_from.clone(),
                candidate.window_to.clone(),
            ))
            .or_default()
            .push(candidate);
    }
    let mut text = String::new();
    let days = args.fold / DAY;
    let chosen = match args.by {
        Some(key) => format!("value of `{key}`, averaged over its runs,"),
        None => "setting".to_string(),
    };
    let _ = writeln!(
        text,
        "Walk-forward over periods of {days} day(s): each period trades the {chosen} that made \
         most over the periods before it (cash when even that one lost), and is scored on what \
         it then made. Results in the value token, in percent, against holding it."
    );
    let (mut held, mut scored_rules) = (0, 0);
    for ((pool, value, from, to), group) in &groups {
        let start = group
            .iter()
            .filter_map(|c| c.curve.first().map(|p| p.0))
            .min()
            .unwrap_or(0);
        let end = group
            .iter()
            .filter_map(|c| c.curve.last().map(|p| p.0))
            .max()
            .unwrap_or(0);
        let folds = ((end.saturating_sub(start)) / args.fold.max(1)) as usize;
        let units = units(group, args.by)?;
        // A unit's return over a period is the average of its runs'.
        let returns: Vec<Vec<f64>> = units
            .iter()
            .map(|(_, members)| {
                let each: Vec<Vec<f64>> = members
                    .iter()
                    .map(|c| fold_returns(&c.curve, start, args.fold, folds))
                    .collect();
                (0..folds)
                    .map(|fold| each.iter().map(|r| r[fold]).sum::<f64>() / each.len() as f64)
                    .collect()
            })
            .collect();
        let name = if args.across_pools {
            let mut pools: Vec<&str> = group.iter().map(|c| c.pool.as_str()).collect();
            pools.sort_unstable();
            pools.dedup();
            let mut tokens: Vec<&str> = group.iter().map(|c| c.value_token.as_str()).collect();
            tokens.sort_unstable();
            tokens.dedup();
            format!(
                "{} together, value token {}",
                pools.join(", "),
                tokens.join(" or ")
            )
        } else {
            format!("{pool}, value token {value}")
        };
        let _ = writeln!(
            text,
            "\n{name}, {from} to {to}: {} run(s), {} to choose from, {folds} period(s)",
            group.len(),
            units.len()
        );
        if units.len() < 2 {
            let _ = writeln!(text, "  one is nothing to choose from");
            continue;
        }
        let Some(report) = walk_forward(&returns, args.fits, args.min_fit) else {
            let _ = writeln!(
                text,
                "  too few periods to fit on and score; shorten --fold or --fit"
            );
            continue;
        };
        let scored = report.folds - report.first_scored;
        let _ = writeln!(
            text,
            "  scored on the last {scored} period(s); the first {} are only fitted on",
            report.first_scored
        );
        let _ = writeln!(
            text,
            "  {:<22} {:>9} {:>10} {:>8} {:>9}",
            "pick", "result %", "periods >0", "in cash", "verdict"
        );
        for rule in &report.rules {
            scored_rules += 1;
            let ok = report.holds_up(rule);
            held += usize::from(ok);
            let _ = writeln!(
                text,
                "  {:<22} {:>9.2} {:>10} {:>8} {:>9}",
                rule.fit.name(),
                rule.result_pct,
                rule.fold_pct.iter().filter(|r| **r > 0.0).count(),
                rule.picks.iter().filter(|p| p.is_none()).count(),
                if ok { "holds up" } else { "does not" }
            );
        }
        let _ = writeln!(
            text,
            "  {:<22} {:>9.2}\n  {:<22} {:>9.2}   {}\n  {:<22} {:>9}   of {}",
            "the average",
            report.average_pct,
            "best with hindsight",
            report.hindsight_pct,
            units[report.hindsight].0,
            "ended above zero",
            report.above_zero,
            units.len()
        );
        if args.by.is_some() {
            // Each value over the scored periods, best first.
            let mut totals: Vec<(f64, &str)> = units
                .iter()
                .zip(&returns)
                .map(|((label, _), r)| {
                    (
                        compound(r[report.first_scored..].iter().copied()) * 100.0,
                        label.as_str(),
                    )
                })
                .collect();
            totals.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (total, label) in totals {
                let _ = writeln!(text, "    {total:>+8.2}   {label}");
            }
        }
        for rule in &report.rules {
            let _ = writeln!(
                text,
                "  next period, {}: {}",
                rule.fit.name(),
                match rule.next {
                    Some(index) => units[index].0.clone(),
                    None => "cash: nothing made money over the fit".to_string(),
                }
            );
        }
    }
    let _ = writeln!(
        text,
        "\n{held} of {scored_rules} pick(s) held up: made money on periods not fitted on, and \
         more than the average. A pick changes at a period's end at no cost and takes over the \
         new runs as they stand, so this flatters it a little; what holds up goes on to a \
         forward run (`window: {{ to: open }}`) and `lpbot select ladder`."
    );
    Ok(text)
}

/// The gates a forward run must pass before it may hold real positions.
/// The four without a default are the owner's to set: a ladder file that
/// leaves one out is refused.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Gates {
    /// Days the run has been forward.
    pub min_days: f64,
    /// Campaigns it has opened.
    pub min_campaigns: i64,
    /// Its result over holding the value token, in percent.
    pub min_result_pct: f64,
    /// Its worst fall from a peak, in percent.
    pub max_drawdown_pct: f64,
    /// An agent's decisions scored over the whole horizon.
    #[serde(default)]
    pub min_decisions: usize,
    /// An agent's mean edge over cash a decision at the horizon, in
    /// percent of the capital. Left out: not asked.
    #[serde(default)]
    pub min_edge_vs_cash_pct: Option<f64>,
    /// The horizon an agent's decisions are judged over, as `lpbot agent
    /// evaluate` scored them.
    #[serde(default = "default_horizon")]
    pub horizon: String,
}

fn default_horizon() -> String {
    "1d".to_string()
}

/// How a track's capital is shared among the candidates that passed.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    /// The least a candidate that passed gets, so every one keeps being
    /// measured with real positions.
    #[serde(default = "default_min_share")]
    pub min_share: f64,
    /// The most one candidate gets. What the caps leave over stays in cash.
    #[serde(default = "default_max_share")]
    pub max_share: f64,
}

fn default_min_share() -> f64 {
    0.1
}

fn default_max_share() -> f64 {
    0.5
}

impl Default for Allocation {
    fn default() -> Self {
        Self {
            min_share: default_min_share(),
            max_share: default_max_share(),
        }
    }
}

/// Candidates that are compared with each other: one value token (or
/// several dollars), one pot of capital.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Track {
    pub name: String,
    pub value_tokens: Vec<String>,
    /// Result folders whose forward runs are the track's candidates.
    pub results: Vec<PathBuf>,
    /// The capital the track shares out, for the plan in dollars.
    #[serde(default)]
    pub capital_usd: Option<f64>,
}

/// A ladder file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LadderFile {
    pub ladder: String,
    pub gates: Gates,
    #[serde(default)]
    pub allocation: Allocation,
    pub tracks: Vec<Track>,
}

/// Reads a ladder file and checks what it asks.
pub fn parse_ladder(content: &str) -> Result<LadderFile> {
    let file: LadderFile = serde_yaml::from_str(content).context(
        "a ladder needs `ladder`, `tracks` and `gates` with min_days, min_campaigns, \
         min_result_pct and max_drawdown_pct: the gates have no defaults",
    )?;
    let share = |value: f64| value.is_finite() && (0.0..=1.0).contains(&value);
    if !share(file.allocation.min_share)
        || !share(file.allocation.max_share)
        || file.allocation.min_share > file.allocation.max_share
        || file.allocation.max_share == 0.0
    {
        bail!(
            "allocation.min_share and allocation.max_share must be shares between 0 and 1, the \
             first at most the second and the second above 0"
        );
    }
    let gates = &file.gates;
    if !(gates.min_days.is_finite() && gates.min_days > 0.0) {
        bail!("gates.min_days must be above 0: a run with no days forward has shown nothing");
    }
    if !(gates.max_drawdown_pct.is_finite() && gates.max_drawdown_pct > 0.0) {
        bail!("gates.max_drawdown_pct must be above 0");
    }
    if !gates.min_result_pct.is_finite() {
        bail!("gates.min_result_pct must be a number");
    }
    if file.tracks.is_empty() {
        bail!("the ladder has no tracks");
    }
    for track in &file.tracks {
        if track.value_tokens.is_empty() || track.results.is_empty() {
            bail!("track `{}` needs `value_tokens` and `results`", track.name);
        }
        if track
            .capital_usd
            .is_some_and(|c| !(c.is_finite() && c > 0.0))
        {
            bail!(
                "track `{}`: capital_usd must be above 0, or left out",
                track.name
            );
        }
    }
    Ok(file)
}

/// An agent's decisions scored over the gates' horizon.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AgentEvidence {
    pub decisions: usize,
    pub mean_edge_vs_cash_pct: Option<f64>,
}

/// The rows of a run's `agent_scores` at `horizon` whose whole horizon has
/// passed inside the history.
pub fn agent_evidence(scores: &Table, horizon: &str) -> AgentEvidence {
    let horizons = texts(scores, "horizon");
    let ended = texts(scores, "ended");
    let times = floats(scores, "time");
    let ends = floats(scores, "end_time");
    let lengths = floats(scores, "horizon_seconds");
    let edges = floats(scores, "edge_vs_cash_pct");
    let mut complete = Vec::new();
    for (row, at) in horizons.iter().enumerate() {
        let whole = match (
            times.get(row).copied().flatten(),
            ends.get(row).copied().flatten(),
            lengths.get(row).copied().flatten(),
        ) {
            (Some(time), Some(end), Some(length)) => end - time >= length,
            _ => false,
        };
        if at.as_deref() == Some(horizon)
            && ended.get(row).cloned().flatten().as_deref() != Some("window_end")
            && whole
        {
            if let Some(edge) = edges.get(row).copied().flatten() {
                complete.push(edge);
            }
        }
    }
    AgentEvidence {
        decisions: complete.len(),
        mean_edge_vs_cash_pct: (!complete.is_empty())
            .then(|| complete.iter().sum::<f64>() / complete.len() as f64),
    }
}

/// Where a candidate stands on the ladder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum Standing {
    /// A backtest: practice, never a reason for real positions.
    Practice,
    /// Forward, with gates still to pass: what is missing.
    Forward { missing: Vec<String> },
    /// Forward and through every gate.
    Passed,
}

/// Holds a candidate against the gates.
pub fn standing(candidate: &Candidate, agent: Option<&AgentEvidence>, gates: &Gates) -> Standing {
    if !candidate.forward {
        return Standing::Practice;
    }
    let mut missing = Vec::new();
    let days = candidate.days();
    if days < gates.min_days {
        missing.push(format!(
            "{:.0} more day(s) forward ({days:.1} of {})",
            (gates.min_days - days).ceil(),
            gates.min_days
        ));
    }
    if candidate.campaigns < gates.min_campaigns {
        missing.push(format!(
            "{} more campaign(s) ({} of {})",
            gates.min_campaigns - candidate.campaigns,
            candidate.campaigns,
            gates.min_campaigns
        ));
    }
    if candidate.result_pct < gates.min_result_pct {
        missing.push(format!(
            "a result of {:+.2}% over holding {} (it is {:+.2}%)",
            gates.min_result_pct, candidate.value_token, candidate.result_pct
        ));
    }
    if candidate.max_drawdown_pct > gates.max_drawdown_pct {
        missing.push(format!(
            "a drawdown within {}% (it reached {:.2}%)",
            gates.max_drawdown_pct, candidate.max_drawdown_pct
        ));
    }
    if candidate.halted {
        missing.push("a run its loss limit did not stop".to_string());
    }
    if candidate.agent.is_some() {
        let evidence = agent.copied().unwrap_or(AgentEvidence {
            decisions: 0,
            mean_edge_vs_cash_pct: None,
        });
        if evidence.decisions < gates.min_decisions {
            missing.push(format!(
                "{} more decision(s) scored over {} ({} of {}; `lpbot agent evaluate \
                 --horizons next,{}`, then `lpbot run`)",
                gates.min_decisions - evidence.decisions,
                gates.horizon,
                evidence.decisions,
                gates.min_decisions,
                gates.horizon
            ));
        }
        if let Some(wanted) = gates.min_edge_vs_cash_pct {
            match evidence.mean_edge_vs_cash_pct {
                Some(edge) if edge >= wanted => {}
                Some(edge) => missing.push(format!(
                    "a mean edge over cash of {wanted:+.3}% a decision (it is {edge:+.3}%)"
                )),
                None => missing.push("decisions scored against cash".to_string()),
            }
        }
    }
    if missing.is_empty() {
        Standing::Passed
    } else {
        Standing::Forward { missing }
    }
}

/// Shares of a track's capital for candidates with these scores (result
/// per day, in percent): in proportion to the score, none below
/// `min_share` and none above `max_share`. Scores of zero or less count as
/// zero; when all are, the shares are equal. With too many candidates for
/// the floor they get equal shares; what the caps leave over is cash.
pub fn shares(scores: &[f64], allocation: Allocation) -> Vec<f64> {
    let n = scores.len();
    if n == 0 {
        return Vec::new();
    }
    let (floor, cap) = (allocation.min_share, allocation.max_share);
    if floor * n as f64 >= 1.0 {
        return vec![(1.0 / n as f64).min(cap); n];
    }
    let raw: Vec<f64> = scores
        .iter()
        .map(|score| {
            if score.is_finite() {
                score.max(0.0)
            } else {
                0.0
            }
        })
        .collect();
    // Share what is left among the candidates not yet pinned, in
    // proportion to their scores. Whatever lands above the cap is pinned
    // there first, since that hands the others more; then whatever lands
    // below the floor, until nothing moves.
    let mut shares = vec![0.0; n];
    let mut pinned = vec![false; n];
    loop {
        let fixed: f64 = (0..n).filter(|i| pinned[*i]).map(|i| shares[i]).sum();
        let left = (1.0 - fixed).max(0.0);
        let free: Vec<usize> = (0..n).filter(|i| !pinned[*i]).collect();
        let total: f64 = free.iter().map(|i| raw[*i]).sum();
        for &i in &free {
            shares[i] = if total > 0.0 {
                left * raw[i] / total
            } else {
                left / free.len() as f64
            };
        }
        let over: Vec<usize> = free.iter().copied().filter(|i| shares[*i] > cap).collect();
        let under: Vec<usize> = free
            .iter()
            .copied()
            .filter(|i| shares[*i] < floor)
            .collect();
        let (to_pin, at) = if !over.is_empty() {
            (over, cap)
        } else if !under.is_empty() {
            (under, floor)
        } else {
            break;
        };
        for i in to_pin {
            shares[i] = at;
            pinned[i] = true;
        }
    }
    shares
}

/// One candidate's line of the plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub run_id: String,
    pub experiment: String,
    pub what: String,
    pub pool: String,
    pub value_token: String,
    pub days: f64,
    pub campaigns: i64,
    pub result_pct: f64,
    pub result_pct_usd: f64,
    pub hold_usd_pct: f64,
    pub max_drawdown_pct: f64,
    pub agent: Option<AgentEvidence>,
    #[serde(flatten)]
    pub standing: Standing,
    /// Its share of the track's capital: 0 unless it passed.
    pub share: f64,
    pub capital_usd: Option<f64>,
}

/// A track's candidates and how its capital is shared.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackPlan {
    pub track: String,
    pub capital_usd: Option<f64>,
    pub lines: Vec<Line>,
    /// The share no candidate takes.
    pub cash_share: f64,
    /// Result folders of the track that hold no runs yet.
    pub empty: Vec<String>,
}

/// The ladder as it stands: what `lpbot select ladder` writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub ladder: String,
    /// When it was worked out, in seconds since 1970.
    pub as_of: u64,
    pub gates: Gates,
    pub allocation: Allocation,
    pub tracks: Vec<TrackPlan>,
}

/// Works out one track from its candidates and their agents' evidence.
pub fn track_plan(
    track: &Track,
    candidates: &[(Candidate, Option<AgentEvidence>)],
    gates: &Gates,
    allocation: Allocation,
) -> TrackPlan {
    let own: Vec<&(Candidate, Option<AgentEvidence>)> = candidates
        .iter()
        .filter(|(c, _)| track.value_tokens.contains(&c.value_token))
        .collect();
    // Each run is named by what differs within its own experiment.
    let what: Vec<String> = own
        .iter()
        .map(|(c, _)| {
            let same: Vec<&Candidate> = own
                .iter()
                .map(|(other, _)| other)
                .filter(|other| other.experiment == c.experiment)
                .collect();
            let at = same.iter().position(|other| other.run_id == c.run_id);
            at.map_or_else(|| c.label(), |at| differing(&same).swap_remove(at))
        })
        .collect();
    let mut lines: Vec<Line> = own
        .iter()
        .zip(what)
        .map(|((c, agent), what)| Line {
            run_id: c.run_id.clone(),
            experiment: c.experiment.clone(),
            what,
            pool: c.pool.clone(),
            value_token: c.value_token.clone(),
            days: c.days(),
            campaigns: c.campaigns,
            result_pct: c.result_pct,
            result_pct_usd: c.result_pct_usd,
            hold_usd_pct: c.hold_usd_pct,
            max_drawdown_pct: c.max_drawdown_pct,
            agent: *agent,
            standing: standing(c, agent.as_ref(), gates),
            share: 0.0,
            capital_usd: None,
        })
        .collect();
    let passed: Vec<usize> = (0..lines.len())
        .filter(|i| lines[*i].standing == Standing::Passed)
        .collect();
    // A result per day, so runs that began on different days can share.
    let scores: Vec<f64> = passed
        .iter()
        .map(|i| lines[*i].result_pct / lines[*i].days.max(1.0))
        .collect();
    for (index, share) in passed.iter().zip(shares(&scores, allocation)) {
        lines[*index].share = share;
        lines[*index].capital_usd = track.capital_usd.map(|capital| capital * share);
    }
    let cash_share = (1.0 - lines.iter().map(|l| l.share).sum::<f64>()).max(0.0);
    TrackPlan {
        track: track.name.clone(),
        capital_usd: track.capital_usd,
        lines,
        cash_share,
        empty: Vec::new(),
    }
}

/// Reads the forward and practice runs of every track and works out the plan.
pub fn ladder(file: &LadderFile, as_of: u64) -> Result<Plan> {
    let mut tracks = Vec::new();
    for track in &file.tracks {
        let mut candidates = Vec::new();
        let mut empty = Vec::new();
        for results in &track.results {
            // A panel or an experiment that has not run yet is not an error.
            if !results.join("runs").is_dir() {
                empty.push(results.display().to_string());
                continue;
            }
            for candidate in
                read_candidates(results, None).with_context(|| format!("track `{}`", track.name))?
            {
                let scores = table_path(results, "agent_scores", &candidate.run_id);
                let evidence = (candidate.agent.is_some() && scores.exists())
                    .then(|| read_table(&scores))
                    .transpose()?
                    .map(|table| agent_evidence(&table, &file.gates.horizon));
                candidates.push((candidate, evidence));
            }
        }
        tracks.push(TrackPlan {
            empty,
            ..track_plan(track, &candidates, &file.gates, file.allocation)
        });
    }
    Ok(Plan {
        ladder: file.ladder.clone(),
        as_of,
        gates: file.gates.clone(),
        allocation: file.allocation,
        tracks,
    })
}

/// The plan as text.
pub fn plan_text(plan: &Plan) -> String {
    let mut text = String::new();
    let gates = &plan.gates;
    let _ = writeln!(
        text,
        "Ladder `{}`. A forward run may hold real positions once it has {} day(s) forward, {} \
         campaign(s), a result of at least {:+.2}% over holding its value token and a drawdown \
         within {}%{}.",
        plan.ladder,
        gates.min_days,
        gates.min_campaigns,
        gates.min_result_pct,
        gates.max_drawdown_pct,
        match (gates.min_decisions, gates.min_edge_vs_cash_pct) {
            (0, None) => String::new(),
            (n, edge) => format!(
                "; an agent also {n} decision(s) scored over {}{}",
                gates.horizon,
                edge.map(|e| format!(" with a mean edge over cash of at least {e:+.3}%"))
                    .unwrap_or_default()
            ),
        }
    );
    for track in &plan.tracks {
        let forward = track
            .lines
            .iter()
            .filter(|l| l.standing != Standing::Practice)
            .count();
        let _ = writeln!(
            text,
            "\nTrack `{}`: {} forward run(s), {} backtest(s) kept as practice",
            track.track,
            forward,
            track.lines.len() - forward
        );
        for folder in &track.empty {
            let _ = writeln!(
                text,
                "  {folder} holds no runs yet; `lpbot run <experiment> --results-dir {folder}` \
                 writes them"
            );
        }
        if forward > 0 {
            let _ = writeln!(
                text,
                "  {:<40} {:<16} {:>6} {:>6} {:>9} {:>8} {:>7}  standing",
                "run", "pool", "days", "camps", "result %", "max dd %", "share"
            );
        }
        for line in track
            .lines
            .iter()
            .filter(|l| l.standing != Standing::Practice)
        {
            let standing = match &line.standing {
                Standing::Passed => "passed".to_string(),
                Standing::Forward { missing } => format!("needs {}", missing.join("; ")),
                Standing::Practice => unreachable!("practice runs are filtered out"),
            };
            let _ = writeln!(
                text,
                "  {:<40} {:<16} {:>6.1} {:>6} {:>9.2} {:>8.2} {:>6.0}%  {}\n    {}",
                line.run_id,
                line.pool,
                line.days,
                line.campaigns,
                line.result_pct,
                line.max_drawdown_pct,
                line.share * 100.0,
                standing,
                line.what
            );
        }
        let taken: Vec<&Line> = track.lines.iter().filter(|l| l.share > 0.0).collect();
        if taken.is_empty() {
            let _ = writeln!(text, "  Nothing has passed: the track stays in cash.");
        } else {
            let _ = writeln!(
                text,
                "  Plan: {}; {:.0}% stays in cash.",
                taken
                    .iter()
                    .map(|l| format!(
                        "{:.0}% to {}{}",
                        l.share * 100.0,
                        l.run_id,
                        l.capital_usd
                            .map(|c| format!(" (${c:.0})"))
                            .unwrap_or_default()
                    ))
                    .collect::<Vec<_>>()
                    .join(", "),
                track.cash_share * 100.0
            );
        }
    }
    let _ = writeln!(
        text,
        "\nForward runs began on different days: a share follows each one's result per day, not \
         a ranking over the same market. The executor is not built, so nothing is sent: this is \
         the plan a live run would take, for the owner to approve."
    );
    text
}

/// `lpbot select ladder`: prints the plan and writes it under `out`.
pub fn run_ladder(file: &Path, out: &Path, as_of: u64) -> Result<String> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("reading the ladder file {}", file.display()))?;
    let ladder_file = parse_ladder(&content).with_context(|| file.display().to_string())?;
    let plan = ladder(&ladder_file, as_of)?;
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let path = out.join(format!("{}.json", plan.ladder));
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_string_pretty(&plan)?)
        .and_then(|()| std::fs::rename(&temporary, &path))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(format!(
        "{}\nWritten to {}.\n",
        plan_text(&plan),
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_sim::tables::write_table;

    fn candidate(run_id: &str, forward: bool) -> Candidate {
        Candidate {
            run_id: run_id.to_string(),
            experiment: "x".to_string(),
            pool: "USDC/WETH 0.05".to_string(),
            value_token: "USDC".to_string(),
            sweep: format!("setting={run_id}"),
            window_from: "2026-01-01".to_string(),
            window_to: "2026-03-01".to_string(),
            forward,
            agent: None,
            result_pct: 2.0,
            result_pct_usd: 2.0,
            hold_usd_pct: 0.0,
            campaigns: 8,
            max_drawdown_pct: 3.0,
            halted: false,
            curve: vec![(0, 100.0), (40 * DAY, 102.0)],
        }
    }

    fn gates() -> Gates {
        Gates {
            min_days: 30.0,
            min_campaigns: 5,
            min_result_pct: 0.0,
            max_drawdown_pct: 10.0,
            min_decisions: 0,
            min_edge_vs_cash_pct: None,
            horizon: "1d".to_string(),
        }
    }

    #[test]
    fn fits_are_numbers_of_periods_or_all() {
        assert_eq!(
            parse_fits("3, 6,all").unwrap(),
            vec![Fit::Folds(3), Fit::Folds(6), Fit::All]
        );
        for bad in ["", "0", "three", "3,,x"] {
            assert!(parse_fits(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_curve_is_cut_into_period_returns() {
        // 100 for ten days, 110 at day 10, 99 at day 25.
        let curve = [(0, 100.0), (10 * DAY, 110.0), (25 * DAY, 99.0)];
        let returns = fold_returns(&curve, 0, 10 * DAY, 3);
        assert!((returns[0] - 0.10).abs() < 1e-12);
        // Day 20 still reads the point of day 10.
        assert_eq!(returns[1], 0.0);
        assert!((returns[2] + 0.10).abs() < 1e-12);
        // Before the curve begins it is worth its first point.
        assert_eq!(fold_returns(&curve[1..], 0, 10 * DAY, 1), vec![0.0]);
        assert_eq!(fold_returns(&[], 0, DAY, 2), vec![0.0, 0.0]);
    }

    #[test]
    fn the_pick_is_the_best_of_the_fit_or_cash_when_all_lost() {
        let returns = vec![
            vec![0.05, -0.02, -0.02, 0.01],
            vec![-0.01, 0.03, 0.03, -0.04],
            vec![-0.01, -0.01, -0.01, 0.02],
        ];
        // Over the first period alone the first candidate leads.
        assert_eq!(pick(&returns, Fit::Folds(1), 1), Some(0));
        // Over periods 1 and 2 the second does.
        assert_eq!(pick(&returns, Fit::Folds(2), 3), Some(1));
        assert_eq!(pick(&returns, Fit::All, 3), Some(1));
        // The last period alone: the third.
        assert_eq!(pick(&returns, Fit::Folds(1), 4), Some(2));
        // When every candidate lost over the fit, cash.
        let losers = vec![vec![-0.01, 0.5], vec![-0.02, 0.5]];
        assert_eq!(pick(&losers, Fit::Folds(1), 1), None);
        // A fit longer than the history uses what there is.
        assert_eq!(pick(&returns, Fit::Folds(9), 1), Some(0));
    }

    #[test]
    fn walk_forward_scores_each_pick_on_the_period_after_its_fit() {
        // One candidate is steady; the other alternates and looks best
        // right before each of its bad periods.
        let steady = vec![0.01; 6];
        let swinging = vec![0.02, -0.03, 0.02, -0.03, 0.02, -0.03];
        let report = walk_forward(
            &[steady.clone(), swinging.clone()],
            &[Fit::Folds(1), Fit::All],
            2,
        )
        .unwrap();
        // Both fits are scored from period 2 on: the longest need.
        assert_eq!((report.folds, report.first_scored), (6, 2));
        let last_one = &report.rules[0];
        // Chasing the last period: steady after a bad one, swinging after
        // a good one, so it meets every bad period from 3 on.
        assert_eq!(last_one.picks, vec![Some(0), Some(1), Some(0), Some(1)]);
        let expected = compound([0.01, -0.03, 0.01, -0.03]) * 100.0;
        assert!((last_one.result_pct - expected).abs() < 1e-9);
        assert_eq!(last_one.next, Some(0));
        // Over everything so far the steady one always leads.
        let so_far = &report.rules[1];
        assert_eq!(so_far.picks, vec![Some(0); 4]);
        // Benchmarks over the same four periods.
        let average = compound([0.015, -0.01, 0.015, -0.01]) * 100.0;
        assert!((report.average_pct - average).abs() < 1e-9);
        assert_eq!(report.hindsight, 0);
        assert!((report.hindsight_pct - compound([0.01; 4]) * 100.0).abs() < 1e-9);
        assert_eq!(report.above_zero, 1);
        assert!(!report.holds_up(last_one));
        assert!(report.holds_up(so_far));

        // Nothing left to score, or lists of different lengths.
        assert_eq!(
            walk_forward(std::slice::from_ref(&steady), &[Fit::Folds(6)], 1),
            None
        );
        assert_eq!(
            walk_forward(&[steady, swinging[..3].to_vec()], &[Fit::All], 1),
            None
        );
        assert_eq!(walk_forward(&[], &[Fit::All], 1), None);
    }

    #[test]
    fn a_backtest_is_practice_and_a_forward_run_must_pass_every_gate() {
        assert_eq!(
            standing(&candidate("a", false), None, &gates()),
            Standing::Practice
        );
        assert_eq!(
            standing(&candidate("a", true), None, &gates()),
            Standing::Passed
        );
        let young = Candidate {
            curve: vec![(0, 100.0), (12 * DAY, 101.0)],
            campaigns: 2,
            result_pct: -0.5,
            max_drawdown_pct: 12.0,
            halted: true,
            ..candidate("b", true)
        };
        let Standing::Forward { missing } = standing(&young, None, &gates()) else {
            panic!("a run short of every gate has not passed");
        };
        assert_eq!(missing.len(), 5, "{missing:?}");
        assert!(
            missing[0].starts_with("18 more day(s) forward"),
            "{missing:?}"
        );
        assert!(missing[1].starts_with("3 more campaign(s)"), "{missing:?}");
        assert!(missing[2].contains("it is -0.50%"), "{missing:?}");
        assert!(missing[3].contains("it reached 12.00%"), "{missing:?}");
    }

    #[test]
    fn an_agent_also_needs_scored_decisions_and_an_edge_over_cash() {
        let agent = Candidate {
            agent: Some(("a".into(), "claude-opus-5-5".into())),
            ..candidate("panel-000", true)
        };
        assert_eq!(agent.label(), "agent a (claude-opus-5-5)");
        let asked = Gates {
            min_decisions: 10,
            min_edge_vs_cash_pct: Some(0.0),
            ..gates()
        };
        // Not scored at all.
        let Standing::Forward { missing } = standing(&agent, None, &asked) else {
            panic!("an agent without scores has not passed");
        };
        assert!(
            missing[0].starts_with("10 more decision(s) scored over 1d"),
            "{missing:?}"
        );
        assert_eq!(missing[1], "decisions scored against cash");
        // Enough decisions, but they lost to cash.
        let losing = AgentEvidence {
            decisions: 12,
            mean_edge_vs_cash_pct: Some(-0.02),
        };
        let Standing::Forward { missing } = standing(&agent, Some(&losing), &asked) else {
            panic!("an agent that loses to cash has not passed");
        };
        assert_eq!(missing.len(), 1);
        assert!(missing[0].contains("it is -0.020%"), "{missing:?}");
        let winning = AgentEvidence {
            decisions: 12,
            mean_edge_vs_cash_pct: Some(0.03),
        };
        assert_eq!(standing(&agent, Some(&winning), &asked), Standing::Passed);
        // A rule is not asked for decisions.
        assert_eq!(
            standing(&candidate("rule", true), None, &asked),
            Standing::Passed
        );
    }

    #[test]
    fn only_decisions_scored_over_the_whole_horizon_count() {
        let text =
            |values: &[&str]| Column::Text(values.iter().map(|v| Some(v.to_string())).collect());
        let float = |values: &[f64]| Column::Float(values.iter().map(|v| Some(*v)).collect());
        let int = |values: &[i64]| Column::Int(values.iter().map(|v| Some(*v)).collect());
        let day = DAY as i64;
        let scores = Table {
            name: "agent_scores".into(),
            columns: vec![
                ("horizon".into(), text(&["1d", "1d", "next", "1d", "1d"])),
                (
                    "ended".into(),
                    text(&["look", "look", "look", "window_end", "look"]),
                ),
                ("time".into(), int(&[0, day, day, 2 * day, 3 * day])),
                // The last one ends half a day in: not whole.
                (
                    "end_time".into(),
                    int(&[day, 2 * day, 2 * day, 3 * day, 3 * day + day / 2]),
                ),
                ("horizon_seconds".into(), int(&[day; 5])),
                (
                    "edge_vs_cash_pct".into(),
                    float(&[0.10, -0.04, 9.0, 9.0, 9.0]),
                ),
            ],
        };
        let evidence = agent_evidence(&scores, "1d");
        assert_eq!(evidence.decisions, 2);
        assert!((evidence.mean_edge_vs_cash_pct.unwrap() - 0.03).abs() < 1e-12);
        assert_eq!(
            agent_evidence(&scores, "4h"),
            AgentEvidence {
                decisions: 0,
                mean_edge_vs_cash_pct: None
            }
        );
    }

    #[test]
    fn shares_follow_the_scores_within_the_floor_and_the_cap() {
        let bounds = Allocation {
            min_share: 0.1,
            max_share: 0.5,
        };
        let near = |shares: &[f64], wanted: &[f64]| {
            shares.len() == wanted.len()
                && shares.iter().zip(wanted).all(|(a, b)| (a - b).abs() < 1e-9)
        };
        assert!(shares(&[], bounds).is_empty());
        // One candidate is capped; the rest is cash.
        assert!(near(&shares(&[0.3], bounds), &[0.5]));
        // In proportion when inside the bounds.
        assert!(near(&shares(&[0.3, 0.2], bounds), &[0.5, 0.5]));
        assert!(near(&shares(&[3.0, 2.0, 5.0], bounds), &[0.3, 0.2, 0.5]));
        // A dominant score is capped and the others share the rest; a
        // score of nothing still gets the floor.
        let capped = shares(&[10.0, 1.0, 1.0, 0.0], bounds);
        assert!(near(&capped, &[0.5, 0.2, 0.2, 0.1]), "{capped:?}");
        // No score above zero: equal shares.
        assert!(near(
            &shares(&[0.0, -1.0, f64::NAN], bounds),
            &[1.0 / 3.0; 3]
        ));
        // More candidates than the floor allows: equal shares.
        let many = shares(&[1.0; 20], bounds);
        assert!(near(&many, &[0.05; 20]));
        for case in [&[0.3][..], &[10.0, 1.0, 1.0, 0.0], &[1.0; 20]] {
            assert!(shares(case, bounds).iter().sum::<f64>() <= 1.0 + 1e-9);
        }
    }

    #[test]
    fn a_track_shares_its_capital_among_the_runs_that_passed() {
        let track = Track {
            name: "dollars".into(),
            value_tokens: vec!["USDC".into(), "USDT".into()],
            results: vec!["runs/forward".into()],
            capital_usd: Some(10_000.0),
        };
        let strong = Candidate {
            result_pct: 6.0,
            ..candidate("strong", true)
        };
        let eth = Candidate {
            value_token: "WETH".into(),
            ..candidate("eth", true)
        };
        let young = Candidate {
            curve: vec![(0, 100.0), (5 * DAY, 101.0)],
            ..candidate("young", true)
        };
        let candidates = vec![
            (strong, None),
            (candidate("steady", true), None),
            (candidate("old", false), None),
            (eth, None),
            (young, None),
        ];
        let plan = track_plan(&track, &candidates, &gates(), Allocation::default());
        // The WETH run belongs to another track.
        let ids: Vec<&str> = plan.lines.iter().map(|l| l.run_id.as_str()).collect();
        assert_eq!(ids, ["strong", "steady", "old", "young"]);
        // 6% and 2% over the same 40 days: three to one, capped at a half.
        let share = |id: &str| plan.lines.iter().find(|l| l.run_id == id).unwrap().share;
        assert!((share("strong") - 0.5).abs() < 1e-9);
        assert!((share("steady") - 0.5).abs() < 1e-9);
        assert_eq!((share("old"), share("young")), (0.0, 0.0));
        assert_eq!(plan.lines[0].capital_usd, Some(5_000.0));
        assert!(plan.cash_share.abs() < 1e-9);

        let text = plan_text(&Plan {
            ladder: "main".into(),
            as_of: 0,
            gates: gates(),
            allocation: Allocation::default(),
            tracks: vec![plan],
        });
        assert!(
            text.contains("3 forward run(s), 1 backtest(s) kept as practice"),
            "{text}"
        );
        assert!(text.contains("needs 25 more day(s) forward"), "{text}");
        assert!(text.contains("50% to strong ($5000)"), "{text}");
        assert!(text.contains("nothing is sent"), "{text}");

        // With nothing through the gates the track stays in cash.
        let empty = track_plan(&track, &candidates[2..3], &gates(), Allocation::default());
        assert_eq!(empty.cash_share, 1.0);
    }

    #[test]
    fn runs_are_chosen_one_by_one_or_by_the_value_of_a_swept_setting() {
        let with = |run_id: &str, sweep: &str| Candidate {
            sweep: sweep.to_string(),
            ..candidate(run_id, false)
        };
        let runs = [
            with("r0", "entry.width_ticks=600; exit.max_hold=1d; regime={}"),
            with("r1", "entry.width_ticks=600; exit.max_hold=3d; regime={}"),
            with("r2", "entry.width_ticks=600; exit.max_hold=1d; regime={trend_window: 30d, trend_min_pct: 0}"),
        ];
        let group: Vec<&Candidate> = runs.iter().collect();
        // One by one, named by what differs between them.
        let single = units(&group, None).unwrap();
        assert_eq!(single.len(), 3);
        assert_eq!(single[1].0, "r1 (exit.max_hold=3d; regime={})");
        assert_eq!(single[1].1.len(), 1);
        // By regime: the two ungated runs together.
        let by = units(&group, Some("regime")).unwrap();
        let shape: Vec<(&str, usize)> = by.iter().map(|(l, m)| (l.as_str(), m.len())).collect();
        assert_eq!(
            shape,
            [
                ("regime={}", 2),
                ("regime={trend_window: 30d, trend_min_pct: 0}", 1)
            ]
        );
        // A setting the runs do not sweep is refused by name.
        let refused = units(&group, Some("entry.shape")).unwrap_err().to_string();
        assert!(
            refused.contains("does not sweep `entry.shape`"),
            "{refused}"
        );
        // Runs that differ in nothing are named by their run id and sweep.
        let same = [with("a", "x=1"), with("b", "x=1")];
        let named = units(&same.iter().collect::<Vec<_>>(), None).unwrap();
        assert_eq!(named[0].0, "a (x=1)");
    }

    const LADDER: &str = "ladder: main\ngates: { min_days: 30, min_campaigns: 5, min_result_pct: \
                          0, max_drawdown_pct: 10 }\ntracks:\n  - { name: dollars, value_tokens: \
                          [USDC], results: [runs/forward] }\n";

    #[test]
    fn a_ladder_file_must_set_its_gates() {
        let file = parse_ladder(LADDER).unwrap();
        assert_eq!(file.gates, gates());
        assert_eq!(file.allocation, Allocation::default());
        assert_eq!(file.tracks[0].capital_usd, None);
        // A gate left out is refused, and so is one that asks nothing.
        let without = LADDER.replace("min_days: 30, ", "");
        let refused = format!("{:#}", parse_ladder(&without).unwrap_err());
        assert!(refused.contains("min_days"), "{refused}");
        for (from, to) in [
            ("min_days: 30", "min_days: 0"),
            ("max_drawdown_pct: 10", "max_drawdown_pct: -1"),
            ("results: [runs/forward]", "results: []"),
            (
                "ladder: main",
                "ladder: main\nallocation: { min_share: 0.6, max_share: 0.5 }",
            ),
            ("ladder: main", "ladder: main\nthresholds: {}"),
            ("name: dollars,", "name: dollars, capital_usd: 0,"),
        ] {
            assert!(parse_ladder(&LADDER.replace(from, to)).is_err(), "{to}");
        }
    }

    /// A run's `runs` and `series` tables as `lpbot run` writes them.
    fn store_run(results: &Path, run_id: &str, value: &str, to: &str, equity: &[f64]) {
        let text = |v: &str| Column::Text(vec![Some(v.to_string())]);
        let float = |v: f64| Column::Float(vec![Some(v)]);
        let last = *equity.last().unwrap();
        let runs = Table {
            name: "runs".into(),
            columns: vec![
                ("run_id".into(), text(run_id)),
                ("experiment".into(), text("forward-x")),
                ("pool".into(), text("USDC/WETH 0.05")),
                ("value_token".into(), text(value)),
                ("sweep".into(), text("entry.width_ticks=600")),
                ("window_from".into(), text("2026-01-01")),
                ("window_to".into(), text("2026-03-01")),
                ("result_pct".into(), float(last - 100.0)),
                ("result_pct_usd".into(), float(last - 100.0)),
                ("hold_value_pct_usd".into(), float(0.0)),
                ("campaigns".into(), Column::Int(vec![Some(7)])),
                ("max_drawdown_pct".into(), float(1.5)),
                (
                    "halted_by_loss_limit".into(),
                    Column::Flag(vec![Some(false)]),
                ),
                (
                    "config_yaml".into(),
                    text(&format!(
                        "window: {{ from: 2026-01-01, to: {to} }}\nstrategies:\n  - {{ id: s }}\n"
                    )),
                ),
            ],
        };
        write_table(results, run_id, &runs).unwrap();
        let series = Table {
            name: "series".into(),
            columns: vec![
                (
                    "run_id".into(),
                    Column::Text(vec![Some(run_id.to_string()); equity.len()]),
                ),
                (
                    "time".into(),
                    Column::Int(
                        (0..equity.len())
                            .map(|i| Some((i as u64 * 10 * DAY) as i64))
                            .collect(),
                    ),
                ),
                (
                    "equity".into(),
                    Column::Float(equity.iter().map(|e| Some(*e)).collect()),
                ),
            ],
        };
        write_table(results, run_id, &series).unwrap();
    }

    #[test]
    fn the_ladder_and_the_walk_forward_read_what_a_run_wrote() {
        let root = std::env::temp_dir().join(format!("lpbot-select-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let results = root.join("runs");
        store_run(
            &results,
            "fwd-000",
            "USDC",
            "open",
            &[100.0, 101.0, 102.0, 103.0, 104.0],
        );
        store_run(
            &results,
            "fwd-001",
            "USDC",
            "open",
            &[100.0, 99.0, 98.0, 97.0, 96.0],
        );
        store_run(
            &results,
            "back-000",
            "USDC",
            "2026-03-01",
            &[100.0, 105.0, 110.0, 115.0, 120.0],
        );

        let read = read_candidates(&results, None).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(
            read.iter().map(|c| c.forward).collect::<Vec<_>>(),
            [false, true, true]
        );
        assert_eq!(read[1].curve.len(), 5);
        assert!((read[1].days() - 40.0).abs() < 1e-9);
        assert!(read_candidates(&results, Some("other")).unwrap().is_empty());
        assert!(read_candidates(&root.join("nowhere"), None).is_err());

        let ladder_file = root.join("ladder.yml");
        std::fs::write(
            &ladder_file,
            LADDER.replace("runs/forward", results.to_str().unwrap()),
        )
        .unwrap();
        let text = run_ladder(&ladder_file, &root.join("selection"), 1_800_000_000).unwrap();
        assert!(!text.contains("holds no runs yet"), "{text}");
        assert!(
            text.contains("2 forward run(s), 1 backtest(s) kept as practice"),
            "{text}"
        );
        assert!(text.contains("50% to fwd-000"), "{text}");
        assert!(text.contains("it is -4.00%"), "{text}");
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("selection/main.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(written["as_of"], 1_800_000_000u64);
        assert_eq!(written["tracks"][0]["lines"][1]["stage"], "passed");
        assert_eq!(written["tracks"][0]["lines"][0]["stage"], "practice");
        assert_eq!(written["tracks"][0]["lines"][2]["stage"], "forward");

        // A folder of the track that has not been written yet is named,
        // and the rest of the track is still worked out.
        std::fs::write(
            &ladder_file,
            LADDER.replace(
                "results: [runs/forward]",
                &format!(
                    "results: [{}, {}]",
                    root.join("later").display(),
                    results.display()
                ),
            ),
        )
        .unwrap();
        let text = run_ladder(&ladder_file, &root.join("selection"), 1_800_000_000).unwrap();
        assert!(text.contains("later holds no runs yet"), "{text}");
        assert!(text.contains("50% to fwd-000"), "{text}");

        // All three as settings of one group, in periods of ten days.
        let report = walk_forward_report(&WalkForwardArgs {
            results: &results,
            experiment: None,
            fold: 10 * DAY,
            fits: &[Fit::Folds(1), Fit::All],
            min_fit: 1,
            by: None,
            across_pools: false,
        })
        .unwrap();
        assert!(
            report.contains("3 run(s), 3 to choose from, 4 period(s)"),
            "{report}"
        );
        assert!(
            report.contains("scored on the last 3 period(s)"),
            "{report}"
        );
        assert!(report.contains("2 of 2 pick(s) held up"), "{report}");
        assert!(
            report.contains("next period, best so far: back-000"),
            "{report}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
