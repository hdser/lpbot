//! `lpbot load-looks <experiment>`: each agent's decided looks, from its
//! shared log, into `agent_looks`, for the agents dashboard, without the
//! replays that the results (`lpbot run`) and the scores (`lpbot agent
//! evaluate`) need. A look and its reasoning reach the viewer as soon as
//! the command runs after it was decided; the results and scores follow
//! with the next full load.
//!
//! It is safe to run every minute. A run whose looks in the database match
//! its log is left alone; one that differs has its looks replaced, the
//! way `lpbot load-results` replaces them. It sits outside `lpbot agent`
//! so that a session allowed only the agent commands cannot run it.

use anyhow::{bail, Result};
use lpbot_core::ResolvedRun;
use lpbot_sim::agent_tables::{agent_tables, AgentLookRow};
use lpbot_sim::loader::{load_table, loaded_looks, LoadedLooks};

use crate::agent_log::identity;
use crate::agent_rows::looks;
use crate::load_cmd::connect;

/// What the command did for one agent run.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub agent: String,
    pub model: String,
    /// The decided looks in the agent's log.
    pub looks: usize,
    /// The looks written, or `None` when the database already matched.
    pub written: Option<usize>,
    /// Whether the run has its row in `runs`, without which the dashboard
    /// does not list its experiment yet.
    pub run_loaded: bool,
}

impl Outcome {
    /// One line for the terminal or a cron log. It names counts only,
    /// never what an agent decided or why.
    pub fn describe(&self) -> String {
        let what = match self.written {
            Some(n) => format!("{n} look(s) loaded into agent_looks"),
            None => format!("{} look(s), already loaded", self.looks),
        };
        let note = if self.run_loaded {
            ""
        } else {
            "; its run is not in `runs` yet, so the dashboard lists it after the next \
             `lpbot run` and `lpbot load-results`"
        };
        format!("agent {} ({}): {what}{note}", self.agent, self.model)
    }
}

/// Whether the database holds other looks than the log: another count, or
/// another last look number. Looks are only ever appended, so either
/// differs as soon as one is decided.
pub fn changed(log: &[AgentLookRow], loaded: &LoadedLooks) -> bool {
    i64::try_from(log.len()).ok() != Some(loaded.looks)
        || log.iter().map(|r| r.number).max() != loaded.last
}

/// Loads the looks of every agent run of the experiment that differ from
/// the database. Each run is loaded on its own: one that fails is reported
/// and the others still load.
pub fn run(url: &str, runs: &[ResolvedRun]) -> Result<Vec<(String, Result<Outcome>)>> {
    let mut client = connect(url)?;
    let mut outcomes = Vec::new();
    for run in runs {
        let Some(strategy) = run.config.strategies.first() else {
            continue;
        };
        let Some(who) = identity(strategy)? else {
            continue;
        };
        let outcome = (|| {
            let log = looks(&who)?;
            let loaded = loaded_looks(&mut client, &run.run_id)?;
            let written = if changed(&log, &loaded) {
                let table = agent_tables(&run.run_id, &log, &[]).swap_remove(0);
                Some(load_table(&mut client, &run.run_id, &table)?)
            } else {
                None
            };
            Ok(Outcome {
                agent: who.name.clone(),
                model: who.model.clone(),
                looks: log.len(),
                written,
                run_loaded: loaded.run_loaded,
            })
        })();
        outcomes.push((who.name.clone(), outcome));
    }
    if outcomes.is_empty() {
        bail!("no run of the experiment is decided by an agent (agent.decisions)");
    }
    Ok(outcomes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look(number: i64) -> AgentLookRow {
        AgentLookRow {
            agent: "a".into(),
            number,
            ..AgentLookRow::default()
        }
    }

    fn loaded(looks: i64, last: Option<i64>) -> LoadedLooks {
        LoadedLooks {
            run_loaded: true,
            looks,
            last,
        }
    }

    #[test]
    fn looks_load_only_when_the_database_differs_from_the_log() {
        assert!(!changed(&[], &loaded(0, None)));
        assert!(changed(&[look(1)], &loaded(0, None)));
        assert!(!changed(&[look(1), look(2)], &loaded(2, Some(2))));
        // A new look, a log started again, and looks the log no longer has.
        assert!(changed(&[look(1), look(2), look(3)], &loaded(2, Some(2))));
        assert!(changed(&[look(1), look(2)], &loaded(2, Some(5))));
        assert!(changed(&[], &loaded(3, Some(3))));
    }

    #[test]
    fn each_line_gives_counts_and_says_when_the_run_is_not_listed_yet() {
        let mut outcome = Outcome {
            agent: "a".into(),
            model: "claude-opus-5-5".into(),
            looks: 3,
            written: Some(3),
            run_loaded: true,
        };
        assert_eq!(
            outcome.describe(),
            "agent a (claude-opus-5-5): 3 look(s) loaded into agent_looks"
        );
        outcome.written = None;
        outcome.run_loaded = false;
        let line = outcome.describe();
        assert!(line.starts_with("agent a (claude-opus-5-5): 3 look(s), already loaded; its run is not in `runs` yet"), "{line}");
    }

    #[test]
    fn a_database_that_is_not_there_is_reported_without_its_password() {
        // Port 9 has no listener.
        let url = "postgres://lpbot:topsecret@127.0.0.1:9/lpbot";
        let error = run(url, &[]).unwrap_err().to_string();
        assert!(error.contains("cannot connect to the database"), "{error}");
        assert!(!error.contains("topsecret"), "{error}");
    }
}
