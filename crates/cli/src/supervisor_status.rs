//! `lpbot supervisor status`: every forward agent on this machine, what it
//! is doing and whether anything will wake it, in one table.
//!
//! The state comes from what is on disk and what runs: each agent's
//! decisions (the canonical due time of its next look), its pending look,
//! its ownership fence, the runner's state file, the agent list
//! (`supervisor_list`) and the agent processes (`supervisor_procs`). Activity
//! is kept apart from health: a waiter, a fence or a saved runner state
//! shows that something ran, not that a scheduler works, so the scheduler
//! has its own column.
//!
//! It writes nothing next to a run: only its own memory of when each look
//! was first seen shown (lateness is always counted from the due time, and
//! showing a look again does not reset it), and, with `--load`, the
//! `agent_status` table. The fence's token is never read into any output.
//!
//! It sits outside `lpbot agent`, like `load-looks`, so that a session
//! allowed only the agent commands cannot see every agent's state now,
//! outside the rule about time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context, Result};
use clap::Args as ClapArgs;
use lpbot_core::{resolve_experiment, ResolvedRun, RunOverrides};
use lpbot_sim::agent::AgentAction;
use lpbot_sim::supervisor_db::replace_status;
use lpbot_sim::supervisor_tables::{agent_status_table, AgentStatusRow};
use serde_json::{json, Value};

use crate::agent_board::{board_path, read_board};
use crate::agent_cmd::{iso, read_pending};
use crate::agent_lock;
use crate::agent_log::{identity, Identity};
use crate::run_cmd::{parse_decisions, parse_duration, run_window};
use crate::supervisor_list::{normal, AgentList, Checked, Listed, ListedAgent, Runner};
use crate::supervisor_procs::{agent_proc, runner_proc, AgentProc, Kind, Processes};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The agent list. Defaults to data/agents/agents.yml, which may be
    /// missing; a list named here must exist.
    #[arg(long)]
    pub list: Option<PathBuf>,
    /// Folder of the experiment files.
    #[arg(long, default_value = "experiments")]
    pub experiments: PathBuf,
    /// How long a due look may wait for its decision before it is late.
    /// Overrides the list's `grace`.
    #[arg(long)]
    pub grace: Option<String>,
    /// The rows as JSON.
    #[arg(long, conflicts_with = "brief")]
    pub json: bool,
    /// One line an agent.
    #[arg(long)]
    pub brief: bool,
    /// Replace the rows of the agent_status table.
    #[arg(long)]
    pub load: bool,
    /// Database to load into, as postgres://user:password@host:port/name.
    /// Defaults to LPBOT_DB_URL.
    #[arg(long)]
    pub db: Option<String>,
    /// Print only the agents whose state changed since the last pass, one
    /// line each, for a cron log.
    #[arg(long)]
    pub changes: bool,
    /// Where the status keeps its memory between passes.
    #[arg(long, default_value = "logs/.agent-status-state.json")]
    pub memory: PathBuf,
}

/// What an agent is doing, as the status tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Waiting,
    Unverified,
    Deciding,
    Due,
    Late,
    NoWaiter,
    Stopped,
    Failing,
    SchedulerDown,
    Paused,
    Ended,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Unverified => "unverified",
            State::Deciding => "deciding",
            State::Due => "due",
            State::Late => "late",
            State::NoWaiter => "no waiter",
            State::Stopped => "stopped",
            State::Failing => "failing",
            State::SchedulerDown => "scheduler down",
            State::Paused => "paused",
            State::Ended => "ended",
        }
    }

    /// States that need someone: the dashboard shows them in red.
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            State::Late | State::NoWaiter | State::Stopped | State::Failing | State::SchedulerDown
        )
    }
}

/// What schedules an agent's looks, and how alive it looks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scheduler {
    /// A session. `here`: it runs on this machine and keeps
    /// `agent next --wait` between looks.
    Session { here: bool },
    /// `lpbot agent-runner`: whether its process was found.
    Runner { process: bool },
}

/// Everything `classify` decides from. Times are Unix seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub now: u64,
    pub grace: u64,
    pub listed: Listed,
    /// Why the decisions could not be read, if they could not.
    pub read_error: Option<String>,
    /// The runner's phase, when the runner has stopped the agent.
    pub stopped: Option<String>,
    pub runner_error: Option<String>,
    /// Whether a runner's ownership fence is beside the decisions.
    pub owned: bool,
    pub scheduler: Scheduler,
    /// The canonical due time of the next look.
    pub due: u64,
    /// The next look's number.
    pub look: u64,
    /// When that look was first seen shown, if it is shown.
    pub first_shown: Option<u64>,
    /// When the latest `agent next --wait` for the agent ends.
    pub next_until: Option<u64>,
    /// When the questions an `agent answers --wait` waits on are due.
    pub answers_until: Option<u64>,
    /// Whether any other agent command runs for the agent.
    pub busy: bool,
}

/// A time for people: `2026-11-26 22:58 UTC`.
pub fn clock(time: u64) -> String {
    let iso = iso(time);
    format!("{} {} UTC", &iso[..10], &iso[11..16])
}

/// A length of time for people: seconds, minutes, hours or days.
pub fn span(seconds: u64) -> String {
    if seconds < 90 {
        format!("{seconds} s")
    } else if seconds < 90 * 60 {
        format!("{} min", (seconds + 30) / 60)
    } else if seconds < 48 * 3_600 {
        format!("{:.1} h", seconds as f64 / 3_600.0)
    } else {
        format!("{:.1} d", seconds as f64 / 86_400.0)
    }
}

/// The agent's state and one sentence about it.
pub fn classify(f: &Facts) -> (State, String) {
    match &f.listed {
        Listed::Paused(reason) => return (State::Paused, reason.clone()),
        Listed::Ended(reason) => return (State::Ended, reason.clone()),
        Listed::Active => {}
    }
    if let Some(error) = &f.read_error {
        return (
            State::Failing,
            format!("its decisions cannot be read: {error}"),
        );
    }
    if let Some(phase) = &f.stopped {
        return (
            State::Stopped,
            format!("stopped by the runner ({phase}): inspect before releasing it"),
        );
    }
    if f.owned && f.scheduler == (Scheduler::Runner { process: false }) {
        return (
            State::SchedulerDown,
            "held by lpbot agent-runner, which is not running: nothing will wake it".into(),
        );
    }
    if let Some(error) = &f.runner_error {
        return (State::Failing, format!("the runner's last error: {error}"));
    }
    let look = f.look;
    let due = clock(f.due);
    if f.now < f.due {
        let ahead = format!("look {look}, due {due} (in {})", span(f.due - f.now));
        let waiter = f.next_until.filter(|until| *until >= f.due);
        return match (&f.scheduler, waiter) {
            (Scheduler::Runner { process: true }, _) => (
                State::Waiting,
                format!("waiting for {ahead}; held by lpbot agent-runner (process found)"),
            ),
            (_, Some(until)) => (
                State::Waiting,
                format!(
                    "waiting for {ahead}; agent next --wait runs until {}",
                    clock(until)
                ),
            ),
            (Scheduler::Session { here: true }, None) => match f.next_until {
                Some(until) => (
                    State::NoWaiter,
                    format!(
                        "its agent next --wait ends at {}, before {ahead}: the session will \
                         not wake for it by itself",
                        clock(until)
                    ),
                ),
                None => (
                    State::NoWaiter,
                    format!(
                        "no agent next --wait is running for {ahead}: the session will not \
                         wake by itself"
                    ),
                ),
            },
            _ => (
                State::Unverified,
                format!("{ahead}; run from elsewhere, so nothing here shows what will wake it"),
            ),
        };
    }
    let over = f.now - f.due;
    if let Some(until) = f.answers_until.filter(|until| *until > f.now) {
        return (
            State::Deciding,
            format!(
                "deciding look {look}: waiting for answers until {}",
                clock(until)
            ),
        );
    }
    let shown = f.first_shown.map(|t| format!(", first shown {}", clock(t)));
    if over <= f.grace {
        return if shown.is_some() || f.busy {
            (
                State::Deciding,
                format!(
                    "deciding look {look}, due {due}{}",
                    shown.unwrap_or_default()
                ),
            )
        } else {
            (
                State::Due,
                format!("look {look} is due since {due} ({} ago)", span(over)),
            )
        };
    }
    let how = match (&shown, f.busy) {
        (Some(shown), _) => format!("{shown}, not decided"),
        (None, true) => ", a command is running for it".to_string(),
        (None, false) => ", nothing has shown it".to_string(),
    };
    let wake = match (&f.scheduler, f.next_until) {
        (Scheduler::Session { here: true }, None) => "; no agent next --wait is running",
        _ => "",
    };
    (
        State::Late,
        format!(
            "late by {}: look {look} was due at {due}{how}{wake}",
            span(over)
        ),
    )
}

/// The status's memory between passes: when each look was first seen
/// shown, and the state last reported for each run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Memory {
    /// `<run id> <look time>` to the first time it was seen shown.
    pub first_shown: BTreeMap<String, u64>,
    pub states: BTreeMap<String, String>,
}

impl Memory {
    pub fn parse(text: &str) -> Memory {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return Memory::default();
        };
        let map = |key: &str| v.get(key).and_then(Value::as_object).cloned();
        Memory {
            first_shown: map("first_shown")
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k, v.as_u64()?)))
                .collect(),
            states: map("states")
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k, v.as_str()?.to_string())))
                .collect(),
        }
    }

    pub fn to_json(&self) -> String {
        json!({"first_shown": self.first_shown, "states": self.states}).to_string()
    }

    /// When the look at `look` of `run` was first seen shown: the first
    /// sighting, or the pending file's time when that is earlier.
    pub fn shown(&mut self, run: &str, look: u64, now: u64, written: Option<u64>) -> u64 {
        let first = written.map_or(now, |w| w.min(now));
        *self
            .first_shown
            .entry(format!("{run} {look}"))
            .or_insert(first)
    }

    /// Forgets the looks of `run` before `next`, decided since.
    pub fn forget_before(&mut self, run: &str, next: u64) {
        let prefix = format!("{run} ");
        self.first_shown.retain(|k, _| {
            k.strip_prefix(&prefix)
                .and_then(|t| t.parse::<u64>().ok())
                .is_none_or(|t| t >= next)
        });
    }
}

/// One agent run found, with where it came from.
struct Found {
    run: ResolvedRun,
    who: Identity,
    path: PathBuf,
    runs: usize,
    listed: Option<ListedAgent>,
    board: bool,
}

/// Whether the experiment's text says `to: open`, as `refresh.sh` looks.
pub fn mentions_open_window(text: &str) -> bool {
    text.lines().any(|line| {
        line.match_indices("to:").any(|(i, _)| {
            let before = line[..i].chars().last();
            let rest = line[i + 3..].trim_start();
            before.is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
                && rest.starts_with("open")
                && rest[4..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
        })
    })
}

/// A row for an agent the status could not look at properly.
fn problem(experiment: &str, agent: &str, detail: String, listed: bool) -> AgentStatusRow {
    AgentStatusRow {
        run_id: format!("{experiment}/{agent}"),
        experiment: experiment.to_string(),
        agent: agent.to_string(),
        model: "unknown".into(),
        runner: "unknown".into(),
        state: State::Failing.name().into(),
        detail,
        what_to_do: "Read the detail: the experiment or data/agents/agents.yml needs fixing."
            .into(),
        listed,
        scheduler: "unknown".into(),
        ..AgentStatusRow::default()
    }
}

/// The forward agents of the experiments folder and of the list.
fn discover(root: &Path, folder: &Path, list: &Checked) -> (Vec<Found>, Vec<AgentStatusRow>) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(root.join(folder))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "yml" || e == "yaml"))
                .collect()
        })
        .unwrap_or_default();
    for agent in &list.agents {
        let path = normal(root, &agent.experiment);
        if !paths.iter().any(|p| normal(root, p) == path) {
            paths.push(path);
        }
    }
    paths.sort();
    let mut found = Vec::new();
    let mut problems = Vec::new();
    let mut seen_listed = Vec::new();
    for path in paths {
        let listed_here: Vec<&ListedAgent> = list
            .agents
            .iter()
            .filter(|a| normal(root, &a.experiment) == normal(root, &path))
            .collect();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if listed_here.is_empty() && !mentions_open_window(&text) {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let runs = match resolve_experiment(&path, &RunOverrides::default()) {
            Ok(runs) => runs,
            Err(e) => {
                for agent in &listed_here {
                    problems.push(problem(
                        &name,
                        &agent.name,
                        format!("its experiment does not resolve: {e:#}"),
                        true,
                    ));
                    seen_listed.push((normal(root, &path), agent.name.clone()));
                }
                continue;
            }
        };
        let open = runs
            .first()
            .and_then(|r| run_window(&r.config).ok())
            .is_some_and(|(_, to)| to.is_none());
        let identities: Vec<(ResolvedRun, Identity)> = runs
            .iter()
            .filter_map(|run| {
                let strategy = run.config.strategies.first()?;
                Some((run.clone(), identity(strategy).ok()??))
            })
            .collect();
        let count = identities.len();
        for (run, who) in identities {
            let listed = list.find(root, &path, &who.name).cloned();
            let decisions = normal(root, &who.decisions);
            if listed.is_none() && (!open || !decisions.exists()) {
                continue;
            }
            if let Some(l) = &listed {
                seen_listed.push((normal(root, &l.experiment), l.name.clone()));
            }
            let sharing = runs
                .iter()
                .filter_map(|r| identity(r.config.strategies.first()?).ok()?)
                .filter(|other| other.decisions.parent() == who.decisions.parent())
                .count();
            let board = listed.as_ref().and_then(|l| l.board).unwrap_or(sharing > 1);
            found.push(Found {
                run,
                who,
                path: path.clone(),
                runs: count,
                listed,
                board,
            });
        }
    }
    for agent in &list.agents {
        let key = (normal(root, &agent.experiment), agent.name.clone());
        if !seen_listed.contains(&key) {
            let experiment = agent
                .experiment
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();
            problems.push(problem(
                &experiment,
                &agent.name,
                "listed, but its experiment has no agent run of that name".into(),
                true,
            ));
        }
    }
    (found, problems)
}

fn modified(path: &Path) -> Option<u64> {
    let time = std::fs::metadata(path).ok()?.modified().ok()?;
    time.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn as_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The latest deadline of the questions the agent asked at the look.
fn answers_deadline(who: &Identity, look: u64) -> Option<u64> {
    read_board(&board_path(&who.decisions))
        .ok()?
        .iter()
        .filter(|r| {
            r["kind"] == "ask"
                && r["agent"] == who.name.as_str()
                && r["look"].as_u64() == Some(look)
        })
        .filter_map(|r| r["deadline"].as_u64())
        .max()
}

/// The `experiment` of a runner config, absolute.
fn runner_experiment(root: &Path, config: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config).ok()?;
    let v: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
    let experiment = v.get("experiment")?.as_str()?;
    Some(normal(root, Path::new(experiment)))
}

/// Works out one agent's row.
fn row(
    root: &Path,
    found: &Found,
    procs: &[AgentProc],
    runner_panels: &[PathBuf],
    memory: &mut Memory,
    now: u64,
    grace: u64,
) -> AgentStatusRow {
    let who = &found.who;
    let run_id = found.run.run_id.clone();
    let decisions = normal(root, &who.decisions);
    let mut row = AgentStatusRow {
        run_id: run_id.clone(),
        experiment: found.run.experiment.clone(),
        agent: who.name.clone(),
        model: who.model.clone(),
        listed: found.listed.is_some(),
        place: found.listed.as_ref().and_then(|l| l.place.clone()),
        checked: as_i64(now),
        ..AgentStatusRow::default()
    };

    // The decisions, and the due time of the next look.
    let mut read_error = None;
    let text = match agent_lock::read_optional(&decisions) {
        Ok(text) => text,
        Err(e) => {
            read_error = Some(format!("{e:#}"));
            String::new()
        }
    };
    if !text.is_empty() && !text.ends_with('\n') {
        read_error = Some("the last line is unterminated".into());
    }
    let parsed = if read_error.is_none() {
        parse_decisions(&text).unwrap_or_else(|e| {
            read_error = Some(format!("{e:#}"));
            Vec::new()
        })
    } else {
        Vec::new()
    };
    let start = run_window(&found.run.config)
        .map(|(from, _)| from.start())
        .unwrap_or(now);
    let last = parsed.last();
    let due = last.map_or(start, |d| {
        d.at.unwrap_or(d.look).saturating_add(d.next_look)
    });
    let looks = parsed.len() as u64;
    row.looks = as_i64(looks);
    if let Some(d) = last {
        row.last_number = Some(as_i64(looks));
        row.last_time = Some(as_i64(d.at.unwrap_or(d.look)));
        row.last_action = Some(d.action.name().to_string());
        if let AgentAction::Enter {
            lower_pct,
            upper_pct,
        } = d.action
        {
            row.last_range = Some(format!("{lower_pct:+.2}% to {upper_pct:+.2}%"));
        }
    }
    row.due = Some(as_i64(due));

    // The pending look, and when it was first seen shown.
    memory.forget_before(&run_id, due);
    let pending = read_pending(&decisions).ok();
    let first_shown = pending.as_ref().map(|p| {
        let file = agent_lock::sibling(&decisions, ".pending");
        memory.shown(&run_id, p.look, now, modified(&file))
    });
    row.first_shown = first_shown.map(as_i64);

    // The fence: who holds the agent, and since when. Never its token.
    let fence = read_json(&agent_lock::sibling(&decisions, ".runner-owner.json"));
    let owned = fence.is_some();
    if let Some(fence) = &fence {
        row.owner = Some("lpbot agent-runner".into());
        row.owner_since = fence["activated_unix"].as_u64().map(as_i64);
    }

    // The runner's own view of the agent.
    let mut stopped = None;
    let mut runner_error = None;
    let state_file = found.listed.as_ref().and_then(|l| l.runner_state.clone());
    if let Some(state) = state_file
        .as_ref()
        .and_then(|p| read_json(&normal(root, p)))
    {
        let agent = &state["agents"][who.name.as_str()];
        row.runner_phase = agent["phase"].as_str().map(str::to_string);
        row.stopped = agent["stopped"].as_bool();
        if row.stopped == Some(true) {
            stopped = Some(
                row.runner_phase
                    .clone()
                    .unwrap_or_else(|| "no phase".into()),
            );
        }
        runner_error = agent["error"].as_str().map(str::to_string);
        row.last_error = runner_error.clone();
    }

    // Its processes.
    let mine: Vec<&AgentProc> = procs
        .iter()
        .filter(|p| {
            p.experiment == normal(root, &found.path)
                && p.is_for(&who.name, found.run.index, found.runs)
        })
        .collect();
    let latest = |kind: Kind| {
        mine.iter()
            .filter(|p| p.kind == kind)
            .max_by_key(|p| p.until().unwrap_or(0))
            .copied()
    };
    let next = latest(Kind::NextWait);
    let inbox = latest(Kind::InboxWait).filter(|p| p.until().is_some_and(|u| u > now));
    let answers = latest(Kind::AnswersWait);
    let busy = mine.iter().any(|p| p.kind == Kind::Busy);
    let answers_until = answers
        .and(pending.as_ref())
        .and_then(|p| answers_deadline(who, p.look));
    let shown_waiter = next.or(answers).or(inbox);
    if let Some(w) = shown_waiter {
        row.waiter_kind = Some(w.kind.name().to_string());
        row.waiter_pid = Some(i64::from(w.pid));
        row.waiter_until = match w.kind {
            Kind::AnswersWait => answers_until.map(as_i64),
            _ => w.until().map(as_i64),
        };
    }

    // The scheduler, apart from the agent.
    let runner_kind = owned
        || found
            .listed
            .as_ref()
            .is_some_and(|l| l.runner == Runner::Runner);
    let here = found.listed.as_ref().is_some_and(|l| l.waits_here);
    let scheduler = if runner_kind {
        let process = runner_panels.contains(&normal(root, &found.path));
        let saved = state_file.as_ref().and_then(|p| modified(&normal(root, p)));
        row.scheduler = match (process, saved) {
            (true, Some(saved)) => format!(
                "runner: process found, state saved {} ago",
                span(now.saturating_sub(saved))
            ),
            (true, None) => "runner: process found".into(),
            (false, _) => "runner: no process".into(),
        };
        row.scheduler_seen = saved.map(as_i64);
        Scheduler::Runner { process }
    } else {
        row.scheduler = match (here, mine.is_empty()) {
            (_, false) => "session: process found".into(),
            (true, true) => "session here: no process".into(),
            (false, true) => "unverified".into(),
        };
        row.scheduler_seen = mine.iter().filter_map(|p| p.started).max().map(as_i64);
        Scheduler::Session { here }
    };
    let runner = if runner_kind {
        Runner::Runner
    } else {
        Runner::Session
    };
    row.runner = runner.name().to_string();
    if found.board && here {
        row.listening = Some(inbox.is_some());
    }

    let facts = Facts {
        now,
        grace,
        listed: found
            .listed
            .as_ref()
            .map_or(Listed::Active, |l| l.listed.clone()),
        read_error,
        stopped,
        runner_error,
        owned,
        scheduler,
        due,
        look: looks + 1,
        first_shown,
        next_until: next.and_then(AgentProc::until),
        answers_until,
        busy,
    };
    let (state, mut detail) = classify(&facts);
    if row.listening == Some(false) && !matches!(state, State::Paused | State::Ended) {
        detail.push_str("; not listening for questions (no agent inbox --wait)");
    }
    row.state = state.name().to_string();
    row.detail = detail;
    let experiment = found.path.strip_prefix(root).unwrap_or(&found.path);
    row.what_to_do = what_to_do(
        state,
        &facts.scheduler,
        row.place.as_deref(),
        &experiment.display().to_string(),
        &who.name,
    );
    row
}

/// The runner's config on the node machine, as `docs/CODEX_RUNNER.md` sets
/// it up.
const RUNNER_CONFIG: &str = "data/codex/panel-runner.yml";

/// What to do about an agent in `state`, with the commands to type on the
/// node machine, in `~/lpbot`; empty when nothing needs doing. It holds no
/// times, so it stays the same while the state does: an alert keyed by it
/// stays one alert.
pub fn what_to_do(
    state: State,
    scheduler: &Scheduler,
    place: Option<&str>,
    experiment: &str,
    agent: &str,
) -> String {
    let elsewhere = || {
        format!(
            "It runs from elsewhere ({}): resume it there, or mark it paused with a reason in \
             data/agents/agents.yml.",
            place.unwrap_or("not named in data/agents/agents.yml")
        )
    };
    let session = |why: &str| {
        format!(
            "{why} Look at its window: tmux attach -t agent, window {agent}. If its session has ended, \
             start it again, with the owner's yes: tmux new-window -d -t agent -n {agent} \
             deploy/agents/start-agent.sh {experiment} {agent}"
        )
    };
    match (state, scheduler) {
        (State::Late, Scheduler::Session { here: true }) => {
            session("Its session has not decided the due look.")
        }
        (State::Late, Scheduler::Session { here: false }) => elsewhere(),
        (State::Late, Scheduler::Runner { .. }) => format!(
            "Its runner has not decided the due look: read target/release/lpbot agent-runner \
             {RUNNER_CONFIG} --status and journalctl --user -u codex-panel -n 50."
        ),
        (State::NoWaiter, _) => session("Nothing will wake its session."),
        (State::Unverified, _) => format!(
            "Nothing here can wake it: check it where it runs ({}).",
            place.unwrap_or("not named in data/agents/agents.yml")
        ),
        (State::Stopped, _) => format!(
            "The runner stopped it for inspection: read target/release/lpbot agent-runner \
             {RUNNER_CONFIG} --status, and docs/CODEX_RUNNER.md before releasing it."
        ),
        (State::Failing, Scheduler::Runner { .. }) => format!(
            "Read the error: target/release/lpbot agent-runner {RUNNER_CONFIG} --status and \
             journalctl --user -u codex-panel -n 50."
        ),
        (State::Failing, _) => {
            "Read the detail: the decisions file, the experiment or data/agents/agents.yml needs fixing."
                .to_string()
        }
        (State::SchedulerDown, _) => format!(
            "Its runner holds it but is not running: systemctl --user start codex-panel, or release \
             it (target/release/lpbot agent-runner {RUNNER_CONFIG} --release) if it is meant to be off."
        ),
        (State::Waiting | State::Deciding | State::Due | State::Paused | State::Ended, _) => {
            String::new()
        }
    }
}

/// Every agent's row, in a stable order: the ones that need someone first.
pub fn rows(
    root: &Path,
    folder: &Path,
    list: &Checked,
    processes: &dyn Processes,
    memory: &mut Memory,
    now: u64,
    grace: u64,
) -> Result<(Vec<AgentStatusRow>, bool)> {
    let (found, mut out) = discover(root, folder, list);
    let listed = processes.list();
    let seen = listed.is_some();
    let all = listed.unwrap_or_default();
    let procs: Vec<AgentProc> = all.iter().filter_map(agent_proc).collect();
    let runner_panels: Vec<PathBuf> = all
        .iter()
        .filter_map(runner_proc)
        .filter_map(|r| runner_experiment(root, &r.config))
        .collect();
    for f in &found {
        out.push(row(root, f, &procs, &runner_panels, memory, now, grace));
    }
    let expected = as_i64(out.len() as u64);
    for r in &mut out {
        r.expected = expected;
    }
    out.sort_by_key(|r| {
        let attention = !STATES
            .iter()
            .any(|s| s.name() == r.state && s.needs_attention());
        (attention, r.experiment.clone(), r.agent.clone())
    });
    Ok((out, seen))
}

const STATES: [State; 11] = [
    State::Waiting,
    State::Unverified,
    State::Deciding,
    State::Due,
    State::Late,
    State::NoWaiter,
    State::Stopped,
    State::Failing,
    State::SchedulerDown,
    State::Paused,
    State::Ended,
];

/// The line for `--brief` and `make status`.
pub fn brief_line(r: &AgentStatusRow) -> String {
    let line = format!(
        "{} ({}, {}): {}: {}",
        r.agent, r.model, r.experiment, r.state, r.detail
    );
    if r.what_to_do.is_empty() {
        line
    } else {
        format!("{line}\n  what to do: {}", r.what_to_do)
    }
}

/// The rows as a table for the terminal.
pub fn table(rows: &[AgentStatusRow], now: u64) -> String {
    let head = [
        "AGENT",
        "MODEL",
        "EXPERIMENT",
        "STATE",
        "NEXT LOOK",
        "LAST",
        "SCHEDULER",
    ];
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let next = r.due.map_or_else(String::new, |due| {
                let due = u64::try_from(due).unwrap_or(0);
                if due >= now {
                    format!("in {}", span(due - now))
                } else {
                    format!("{} ago", span(now - due))
                }
            });
            let last = match (&r.last_number, &r.last_action) {
                (Some(n), Some(a)) => match &r.last_range {
                    Some(range) => format!("{n}: {a} {range}"),
                    None => format!("{n}: {a}"),
                },
                _ => "none".into(),
            };
            [
                r.agent.clone(),
                r.model.clone(),
                r.experiment.clone(),
                r.state.clone(),
                next,
                last,
                r.scheduler.clone(),
            ]
        })
        .collect();
    let mut widths = head.map(str::len);
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let line = |row: &[String]| {
        row.iter()
            .enumerate()
            .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = line(&head.map(str::to_string));
    out.push('\n');
    for (row, r) in cells.iter().zip(rows) {
        out.push_str(&line(row));
        out.push('\n');
        out.push_str(&format!("  {}\n", r.detail));
        if !r.what_to_do.is_empty() {
            out.push_str(&format!("  what to do: {}\n", r.what_to_do));
        }
    }
    out
}

/// The row as JSON, every column.
pub fn row_json(r: &AgentStatusRow) -> Value {
    json!({
        "run_id": r.run_id, "experiment": r.experiment, "agent": r.agent, "model": r.model,
        "runner": r.runner, "place": r.place, "state": r.state, "detail": r.detail,
        "what_to_do": r.what_to_do,
        "listed": r.listed, "looks": r.looks, "last_number": r.last_number,
        "last_time": r.last_time, "last_action": r.last_action, "last_range": r.last_range,
        "due": r.due, "first_shown": r.first_shown, "waiter_kind": r.waiter_kind,
        "waiter_pid": r.waiter_pid, "waiter_until": r.waiter_until, "listening": r.listening,
        "owner": r.owner, "owner_since": r.owner_since, "scheduler": r.scheduler,
        "scheduler_seen": r.scheduler_seen, "runner_phase": r.runner_phase,
        "stopped": r.stopped, "last_error": r.last_error, "expected": r.expected,
        "checked": r.checked,
    })
}

/// The lines for agents whose state changed since the memory's last pass.
pub fn changes(rows: &[AgentStatusRow], memory: &Memory) -> Vec<String> {
    rows.iter()
        .filter_map(|r| {
            let before = memory.states.get(&r.run_id);
            (before != Some(&r.state)).then(|| {
                format!(
                    "agents: {} ({}): {} -> {} ({})",
                    r.agent,
                    r.experiment,
                    before.map_or("new", String::as_str),
                    r.state,
                    r.detail
                )
            })
        })
        .collect()
}

/// `lpbot supervisor status`.
pub fn run(args: &Args) -> Result<()> {
    let root = std::env::current_dir().context("cannot tell the working folder")?;
    let (path, required) = match &args.list {
        Some(path) => (path.clone(), true),
        None => (PathBuf::from(crate::supervisor_list::DEFAULT_PATH), false),
    };
    let list = AgentList::read(&path, required)?.check(&root)?;
    let grace = match &args.grace {
        Some(text) => parse_duration(text)
            .ok_or_else(|| anyhow!("--grace `{text}` is not a duration such as 10m"))?,
        None => list.grace,
    };
    let now = crate::agent_log::now_unix();
    let lock = agent_lock::exclusive(&agent_lock::sibling(&args.memory, ".lock"), false)
        .with_context(|| format!("cannot lock the status memory {}", args.memory.display()))?;
    let mut memory = Memory::parse(&agent_lock::read_optional(&args.memory).unwrap_or_default());
    let processes = crate::supervisor_procs::LocalProcesses;
    let (rows, seen) = rows(
        &root,
        &args.experiments,
        &list,
        &processes,
        &mut memory,
        now,
        grace,
    )?;
    // The states last reported belong to the change log alone, so that a
    // pass by hand between two cron passes does not swallow a change.
    let changed = changes(&rows, &memory);
    if args.changes {
        memory.states = rows
            .iter()
            .map(|r| (r.run_id.clone(), r.state.clone()))
            .collect();
    }
    agent_lock::atomic(&args.memory, memory.to_json().as_bytes())?;
    drop(lock);

    if args.load {
        let url = crate::load_cmd::database(args.db.clone()).ok_or_else(|| {
            anyhow!(
                "no database given: pass --db or set LPBOT_DB_URL \
                 (`deploy/agents/refresh.sh --looks` takes it from deploy/.env)"
            )
        })?;
        let mut client = crate::load_cmd::connect(&url)?;
        let written = replace_status(&mut client, &agent_status_table(&rows))?;
        if !args.changes {
            println!("loaded {written} row(s) into agent_status");
        }
    }
    if args.changes {
        for line in changed {
            println!("{line}");
        }
        return Ok(());
    }
    if args.json {
        let all: Vec<Value> = rows.iter().map(row_json).collect();
        println!("{}", serde_json::to_string_pretty(&all)?);
    } else if args.brief {
        if rows.is_empty() {
            println!("no forward agent found");
        }
        for r in &rows {
            println!("{}", brief_line(r));
        }
    } else if rows.is_empty() {
        println!("No forward agent found: no experiment with `to: open` has a decisions file, and the agent list names none.");
    } else {
        print!("{}", table(&rows, now));
    }
    if !seen && !args.json {
        eprintln!("note: this machine has no /proc, so no agent process could be seen");
    }
    Ok(())
}

#[cfg(test)]
#[path = "supervisor_status_tests.rs"]
mod tests;
