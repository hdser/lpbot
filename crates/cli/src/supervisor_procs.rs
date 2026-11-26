//! The agent commands running on this machine, read from `/proc`: which
//! agents have a process waiting for their next look, listening for
//! questions or waiting for answers, and until when; and whether
//! `lpbot agent-runner` is running.
//!
//! Nothing in the agent commands writes a heartbeat for this. A process
//! that is gone is simply not found, so a waiter that died cannot leave a
//! stale record behind. On a machine without `/proc` nothing is found and
//! the status says so.

use std::path::{Path, PathBuf};

use crate::run_cmd::parse_duration;

/// A process: its id, its arguments, its working folder and when it
/// started (Unix seconds).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub started: Option<u64>,
}

/// Where processes come from: `/proc` here, a fixed list in tests.
pub trait Processes {
    /// Every process this user can read, or `None` where the machine has
    /// no `/proc` to read.
    fn list(&self) -> Option<Vec<Proc>>;
}

/// The processes of this machine, from `/proc`.
pub struct LocalProcesses;

impl Processes for LocalProcesses {
    fn list(&self) -> Option<Vec<Proc>> {
        let entries = std::fs::read_dir("/proc").ok()?;
        let boot = boot_time();
        let mut procs = Vec::new();
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let dir = entry.path();
            let Ok(raw) = std::fs::read(dir.join("cmdline")) else {
                continue;
            };
            let argv: Vec<String> = raw
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part).into_owned())
                .collect();
            if argv.is_empty() {
                continue;
            }
            let started = std::fs::read_to_string(dir.join("stat"))
                .ok()
                .and_then(|stat| start_ticks(&stat))
                .zip(boot)
                .map(|(ticks, boot)| boot + ticks / TICKS_PER_SECOND);
            procs.push(Proc {
                pid,
                argv,
                cwd: std::fs::read_link(dir.join("cwd")).ok(),
                started,
            });
        }
        Some(procs)
    }
}

/// Clock ticks a second in `/proc/<pid>/stat`: the kernel's USER_HZ, which
/// is 100 on every Linux the node runs.
const TICKS_PER_SECOND: u64 = 100;

fn boot_time() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

/// Field 22 of `/proc/<pid>/stat`, the start in ticks after boot. The
/// command's name (field 2) is in parentheses and may hold spaces, so the
/// fields are counted from the last `)`.
pub fn start_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// What an agent process is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `agent next --wait`: waiting for the next look.
    NextWait,
    /// `agent inbox --wait`: listening for questions.
    InboxWait,
    /// `agent answers --wait`: waiting for answers to its own questions.
    AnswersWait,
    /// Any other agent command: a catch-up, a look being shown or a
    /// decision being written.
    Busy,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::NextWait => "next",
            Kind::InboxWait => "inbox",
            Kind::AnswersWait => "answers",
            Kind::Busy => "busy",
        }
    }
}

/// An `lpbot agent ...` process, read from its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProc {
    pub pid: u32,
    pub kind: Kind,
    pub command: String,
    /// The experiment, absolute.
    pub experiment: PathBuf,
    pub agent: Option<String>,
    pub run: Option<usize>,
    /// The wait's length for `next` and `inbox`.
    pub wait: Option<u64>,
    pub started: Option<u64>,
}

impl AgentProc {
    /// When a `next` or `inbox` wait ends; `answers` ends at its questions'
    /// deadline, which the board holds.
    pub fn until(&self) -> Option<u64> {
        match self.kind {
            Kind::NextWait | Kind::InboxWait => Some(self.started? + self.wait?),
            Kind::AnswersWait | Kind::Busy => None,
        }
    }

    /// Whether this process is for the agent run named `agent`, or, with
    /// no name on its command line, for the run at `index` (or the only
    /// run).
    pub fn is_for(&self, agent: &str, index: usize, runs: usize) -> bool {
        match (&self.agent, self.run) {
            (Some(name), _) => name == agent,
            (None, Some(run)) => run == index,
            (None, None) => runs == 1,
        }
    }
}

/// Flags of the agent commands that take a value, so the value is not
/// taken for the experiment.
const VALUE_FLAGS: &[&str] = &[
    "--capital",
    "--loss-limit",
    "--autonomy",
    "--run",
    "--agent",
    "--chain",
    "--data-dir",
    "--rpc",
    "--max-lag",
    "--to",
    "--question",
    "--ask",
    "--answer",
    "--would",
    "--lower",
    "--upper",
    "--confidence",
    "--action",
    "--next",
    "--note",
    "--reasoning",
    "--expect",
    "--expect-low",
    "--expect-high",
    "--expect-confidence",
    "--stance",
    "--toward",
    "--expected-look",
    "--expected-as-of",
    "--horizons",
    "--jobs",
];

fn is_lpbot(program: &str) -> bool {
    Path::new(program).file_name().and_then(|n| n.to_str()) == Some("lpbot")
}

/// Reads an `lpbot agent <command> <experiment> ...` process. Anything
/// else is `None`.
pub fn agent_proc(proc: &Proc) -> Option<AgentProc> {
    let argv = &proc.argv;
    if !is_lpbot(argv.first()?) || argv.get(1)? != "agent" {
        return None;
    }
    let command = argv.get(2)?.clone();
    let mut experiment = None;
    let mut agent = None;
    let mut run = None;
    let mut wait_value = None;
    let mut wait_flag = false;
    let mut i = 3;
    while i < argv.len() {
        let arg = &argv[i];
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        if flag == "--wait" {
            if command == "answers" {
                wait_flag = true;
            } else {
                let value = inline.or_else(|| {
                    i += 1;
                    argv.get(i).cloned()
                });
                wait_value = value;
            }
        } else if VALUE_FLAGS.contains(&flag) {
            let value = inline.or_else(|| {
                i += 1;
                argv.get(i).cloned()
            });
            match flag {
                "--agent" => agent = value,
                "--run" => run = value.and_then(|v| v.parse().ok()),
                _ => {}
            }
        } else if !arg.starts_with('-') && experiment.is_none() {
            experiment = Some(PathBuf::from(arg));
        }
        i += 1;
    }
    let experiment = experiment?;
    let experiment = match (&proc.cwd, experiment.is_absolute()) {
        (Some(cwd), false) => cwd.join(experiment),
        _ => experiment,
    };
    let experiment = std::fs::canonicalize(&experiment).unwrap_or(experiment);
    let wait = wait_value.as_deref().and_then(parse_duration);
    let kind = match command.as_str() {
        "next" if wait.is_some() => Kind::NextWait,
        "inbox" if wait.is_some() => Kind::InboxWait,
        "answers" if wait_flag => Kind::AnswersWait,
        _ => Kind::Busy,
    };
    Some(AgentProc {
        pid: proc.pid,
        kind,
        command,
        experiment,
        agent,
        run,
        wait,
        started: proc.started,
    })
}

/// An `lpbot agent-runner <config>` coordinator: running, not a one-off
/// `--check`, `--status` or `--release`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerProc {
    pub pid: u32,
    pub config: PathBuf,
    pub started: Option<u64>,
}

pub fn runner_proc(proc: &Proc) -> Option<RunnerProc> {
    let argv = &proc.argv;
    if !is_lpbot(argv.first()?) || argv.get(1)? != "agent-runner" {
        return None;
    }
    if argv
        .iter()
        .any(|a| a == "--check" || a == "--status" || a == "--release")
    {
        return None;
    }
    let config = argv.iter().skip(2).find(|a| !a.starts_with('-'))?;
    let config = PathBuf::from(config);
    let config = match (&proc.cwd, config.is_absolute()) {
        (Some(cwd), false) => cwd.join(config),
        _ => config,
    };
    Some(RunnerProc {
        pid: proc.pid,
        config,
        started: proc.started,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(args: &str) -> Proc {
        Proc {
            pid: 7,
            argv: args.split(' ').map(str::to_string).collect(),
            cwd: Some(PathBuf::from("/home/u/lpbot")),
            started: Some(1_000),
        }
    }

    #[test]
    fn waits_are_told_apart_with_their_ends() {
        let next = agent_proc(&proc(
            "target/release/lpbot agent next experiments/p.yml --agent a --wait 4h",
        ))
        .unwrap();
        assert_eq!(next.kind, Kind::NextWait);
        assert_eq!(next.agent.as_deref(), Some("a"));
        assert_eq!(
            next.experiment,
            PathBuf::from("/home/u/lpbot/experiments/p.yml")
        );
        assert_eq!(next.until(), Some(1_000 + 4 * 3_600));

        let inbox = agent_proc(&proc(
            "/home/u/lpbot/target/release/lpbot agent inbox experiments/p.yml --agent=b --wait=1d12h",
        ))
        .unwrap();
        assert_eq!(inbox.kind, Kind::InboxWait);
        assert_eq!(inbox.agent.as_deref(), Some("b"));
        assert_eq!(inbox.until(), Some(1_000 + 36 * 3_600));

        let answers = agent_proc(&proc(
            "lpbot agent answers experiments/p.yml --agent a --wait",
        ))
        .unwrap();
        assert_eq!(answers.kind, Kind::AnswersWait);
        assert_eq!(answers.until(), None);
    }

    #[test]
    fn other_agent_commands_are_busy_and_the_rest_ignored() {
        let shown = agent_proc(&proc("lpbot agent next /abs/p.yml --run 1 --max-lag 5m")).unwrap();
        assert_eq!(shown.kind, Kind::Busy);
        assert_eq!(shown.run, Some(1));
        assert_eq!(shown.experiment, PathBuf::from("/abs/p.yml"));
        let decide = agent_proc(&proc(
            "lpbot agent decide experiments/p.yml --agent a --action hold --next 4h --note -x",
        ))
        .unwrap();
        assert_eq!(decide.kind, Kind::Busy);
        assert_eq!(decide.command, "decide");
        assert!(agent_proc(&proc("lpbot run experiments/p.yml")).is_none());
        assert!(agent_proc(&proc("/usr/bin/python agent next p.yml")).is_none());
        assert!(agent_proc(&proc("bash -c lpbot agent next p.yml")).is_none());
        // A wait that does not parse is not a wait.
        let odd = agent_proc(&proc("lpbot agent next p.yml --wait soon")).unwrap();
        assert_eq!(odd.kind, Kind::Busy);
    }

    #[test]
    fn a_process_names_its_run_by_agent_or_index() {
        let by_name = agent_proc(&proc("lpbot agent next p.yml --agent a --wait 1h")).unwrap();
        assert!(by_name.is_for("a", 0, 2));
        assert!(!by_name.is_for("b", 1, 2));
        let by_index = agent_proc(&proc("lpbot agent next p.yml --run 1 --wait 1h")).unwrap();
        assert!(by_index.is_for("b", 1, 2));
        assert!(!by_index.is_for("a", 0, 2));
        let alone = agent_proc(&proc("lpbot agent next p.yml --wait 1h")).unwrap();
        assert!(alone.is_for("solo", 0, 1));
        assert!(!alone.is_for("a", 0, 2));
    }

    #[test]
    fn the_runner_is_found_only_while_it_runs() {
        let running = runner_proc(&proc("lpbot agent-runner data/codex/panel-runner.yml")).unwrap();
        assert_eq!(
            running.config,
            PathBuf::from("/home/u/lpbot/data/codex/panel-runner.yml")
        );
        assert!(runner_proc(&proc(
            "lpbot agent-runner data/codex/panel-runner.yml --once"
        ))
        .is_some());
        assert!(runner_proc(&proc("lpbot agent-runner cfg.yml --status")).is_none());
        assert!(runner_proc(&proc("lpbot agent-runner cfg.yml --check")).is_none());
        assert!(runner_proc(&proc("lpbot agent next p.yml")).is_none());
    }

    #[test]
    fn the_start_is_read_past_a_name_with_spaces() {
        let stat = "123 (my (odd) prog) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 4242 99 100";
        assert_eq!(start_ticks(stat), Some(4242));
        assert_eq!(start_ticks("garbage"), None);
    }
}
