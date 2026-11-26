//! The agent list, `data/agents/agents.yml`: what the status cannot tell
//! from an agent's files. Which panels and agents there are (so one that
//! never decided still shows), what runs each one, where, whether a
//! session on this machine keeps a waiter, and whether it is paused or
//! ended, with why.
//!
//! The file is local, like everything under `data/`; the example is
//! `deploy/agents/agents.example.yml`. The fields the status uses are
//! typed; any other key is carried in `extra`, so the file can grow ahead
//! of the code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::run_cmd::parse_duration;

/// The grace a due look gets before it counts as late: time for the
/// catch-up, the sync and the decision.
pub const DEFAULT_GRACE: u64 = 600;

/// Where the agent list is, from the repository root.
pub const DEFAULT_PATH: &str = "data/agents/agents.yml";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentList {
    /// How long a due look may wait for its decision, as a duration.
    #[serde(default)]
    pub grace: Option<String>,
    #[serde(default)]
    pub panels: Vec<PanelEntry>,
    /// Keys the status does not use, kept for the supervisor.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PanelEntry {
    /// The experiment file, from the repository root.
    pub experiment: PathBuf,
    /// Whether the panel's agents ask each other on the board. Unset, the
    /// board counts as on when agents share a folder.
    #[serde(default)]
    pub board: Option<bool>,
    /// The state file of the runner that holds the panel, when one does.
    #[serde(default)]
    pub runner_state: Option<PathBuf>,
    #[serde(default)]
    pub agents: Vec<AgentEntry>,
    /// Keys the status does not use, kept for the supervisor.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AgentEntry {
    pub name: String,
    /// What runs it: `session` or `runner`.
    #[serde(default)]
    pub runner: Option<String>,
    /// Where it runs, in plain words.
    #[serde(default)]
    pub place: Option<String>,
    /// Whether it is a session on this machine that keeps
    /// `agent next --wait` running between looks.
    #[serde(default)]
    pub waits_here: bool,
    /// `active` (the default), `paused` or `ended`.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Keys the status does not use, kept for the supervisor.
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// What runs an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// A session (Claude Code, Codex, a person) that runs the agent
    /// commands itself.
    Session,
    /// `lpbot agent-runner`.
    Runner,
}

impl Runner {
    pub fn name(self) -> &'static str {
        match self {
            Runner::Session => "session",
            Runner::Runner => "runner",
        }
    }
}

/// Whether the owner has set an agent aside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listed {
    Active,
    Paused(String),
    Ended(String),
}

/// One agent as the list describes it, checked.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedAgent {
    pub experiment: PathBuf,
    pub name: String,
    pub runner: Runner,
    pub place: Option<String>,
    pub waits_here: bool,
    pub listed: Listed,
    pub board: Option<bool>,
    pub runner_state: Option<PathBuf>,
}

/// The checked list.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub grace: u64,
    pub agents: Vec<ListedAgent>,
}

impl Checked {
    /// The entry for an agent of an experiment, comparing paths as written
    /// and as resolved from `root`.
    pub fn find(&self, root: &Path, experiment: &Path, name: &str) -> Option<&ListedAgent> {
        let wanted = normal(root, experiment);
        self.agents
            .iter()
            .find(|a| a.name == name && normal(root, &a.experiment) == wanted)
    }
}

/// A path made absolute from `root` and tidied, without touching the disk
/// beyond what `canonicalize` finds.
pub fn normal(root: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

impl AgentList {
    pub fn parse(text: &str) -> Result<AgentList> {
        if text.trim().is_empty() {
            return Ok(AgentList::default());
        }
        serde_yaml::from_str(text).map_err(|e| anyhow!("the agent list is not valid: {e}"))
    }

    /// Reads the list. A missing file is an empty list unless `required`.
    pub fn read(path: &Path, required: bool) -> Result<AgentList> {
        match std::fs::read_to_string(path) {
            Ok(text) => AgentList::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => {
                Ok(AgentList::default())
            }
            Err(e) => Err(anyhow!(
                "cannot read the agent list {}: {e}",
                path.display()
            )),
        }
    }

    /// Checks every entry and names the first one that is wrong.
    pub fn check(&self, root: &Path) -> Result<Checked> {
        let grace = match &self.grace {
            None => DEFAULT_GRACE,
            Some(text) => parse_duration(text)
                .ok_or_else(|| anyhow!("grace: `{text}` is not a duration such as 10m"))?,
        };
        let mut agents: Vec<ListedAgent> = Vec::new();
        for panel in &self.panels {
            if panel.experiment.as_os_str().is_empty() {
                bail!("a panel of the agent list names no experiment");
            }
            let at = || format!("panel {}", panel.experiment.display());
            for agent in &panel.agents {
                if agent.name.trim().is_empty() {
                    bail!("{}: an agent has no name", at());
                }
                let runner = match agent.runner.as_deref() {
                    None | Some("session") => Runner::Session,
                    Some("runner") => Runner::Runner,
                    Some(other) => bail!(
                        "{}, agent {}: runner `{other}` is not one of session, runner",
                        at(),
                        agent.name
                    ),
                };
                let reason = || {
                    agent
                        .reason
                        .clone()
                        .unwrap_or_else(|| "no reason given".to_string())
                };
                let listed = match agent.state.as_deref() {
                    None | Some("active") => Listed::Active,
                    Some("paused") => Listed::Paused(reason()),
                    Some("ended") => Listed::Ended(reason()),
                    Some(other) => bail!(
                        "{}, agent {}: state `{other}` is not one of active, paused, ended",
                        at(),
                        agent.name
                    ),
                };
                let entry = ListedAgent {
                    experiment: panel.experiment.clone(),
                    name: agent.name.clone(),
                    runner,
                    place: agent.place.clone(),
                    waits_here: agent.waits_here,
                    listed,
                    board: panel.board,
                    runner_state: panel.runner_state.clone(),
                };
                let wanted = normal(root, &entry.experiment);
                if agents
                    .iter()
                    .any(|a| a.name == entry.name && normal(root, &a.experiment) == wanted)
                {
                    bail!("{}: agent {} is listed twice", at(), entry.name);
                }
                agents.push(entry);
            }
        }
        Ok(Checked { grace, agents })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../deploy/agents/agents.example.yml");

    #[test]
    fn the_example_parses_and_checks() {
        let list = AgentList::parse(EXAMPLE).unwrap();
        let checked = list.check(Path::new("/nowhere")).unwrap();
        assert_eq!(checked.grace, 600);
        assert!(checked.agents.len() >= 4);
        let a = &checked.agents[0];
        assert_eq!(a.name, "a");
        assert_eq!(a.runner, Runner::Session);
        assert!(a.waits_here);
        assert_eq!(a.board, Some(true));
        assert!(checked
            .agents
            .iter()
            .any(|a| matches!(&a.listed, Listed::Paused(r) if r.contains("peer-score"))));
    }

    #[test]
    fn unknown_keys_are_kept_and_an_empty_file_is_an_empty_list() {
        let list = AgentList::parse(
            "grace: 15m\nsupervisor: {poll: 30s}\npanels:\n  - experiment: e.yml\n    \
             own_record: 5\n    agents:\n      - {name: a, engine: claude}\n",
        )
        .unwrap();
        assert!(list.extra.contains_key("supervisor"));
        assert!(list.panels[0].extra.contains_key("own_record"));
        assert!(list.panels[0].agents[0].extra.contains_key("engine"));
        assert_eq!(list.check(Path::new("/x")).unwrap().grace, 900);
        assert!(AgentList::parse("").unwrap().panels.is_empty());
        let missing = AgentList::read(Path::new("/nowhere/agents.yml"), false).unwrap();
        assert!(missing.panels.is_empty());
        let err = AgentList::read(Path::new("/nowhere/agents.yml"), true).unwrap_err();
        assert!(
            err.to_string().contains("cannot read the agent list"),
            "{err}"
        );
    }

    #[test]
    fn mistakes_are_named() {
        let check = |text: &str| {
            AgentList::parse(text)
                .and_then(|l| l.check(Path::new("/x")))
                .unwrap_err()
                .to_string()
        };
        let twice = check("panels:\n  - experiment: e.yml\n    agents: [{name: a}, {name: a}]\n");
        assert!(twice.contains("agent a is listed twice"), "{twice}");
        let runner =
            check("panels:\n  - experiment: e.yml\n    agents: [{name: a, runner: cron}]\n");
        assert!(runner.contains("runner `cron`"), "{runner}");
        let state = check("panels:\n  - experiment: e.yml\n    agents: [{name: a, state: gone}]\n");
        assert!(state.contains("state `gone`"), "{state}");
        let grace = check("grace: soon\n");
        assert!(grace.contains("grace: `soon`"), "{grace}");
        let unnamed = check("panels:\n  - experiment: e.yml\n    agents: [{name: ''}]\n");
        assert!(unnamed.contains("has no name"), "{unnamed}");
    }

    #[test]
    fn the_same_experiment_written_two_ways_is_one_panel() {
        let dir = std::env::temp_dir().join(format!("lpbot-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("experiments")).unwrap();
        std::fs::write(dir.join("experiments/e.yml"), "x").unwrap();
        let list = AgentList::parse(&format!(
            "panels:\n  - experiment: experiments/e.yml\n    agents: [{{name: a}}]\n  \
             - experiment: {}\n    agents: [{{name: a}}]\n",
            dir.join("experiments/e.yml").display()
        ))
        .unwrap();
        assert!(list.check(&dir).is_err());
        let one = AgentList::parse(
            "panels:\n  - experiment: experiments/e.yml\n    agents: [{name: a}]\n",
        )
        .unwrap()
        .check(&dir)
        .unwrap();
        let abs = dir.join("experiments/e.yml");
        assert!(one.find(&dir, &abs, "a").is_some());
        assert!(one
            .find(&dir, Path::new("experiments/e.yml"), "b")
            .is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
